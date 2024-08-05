use log::{debug, info};
use proc_macro2::Span;
use quote::ToTokens;
use regex::Regex;
use rem_utils::format_source;
use serde::{Deserialize, Serialize};
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::process::Command;
use syn::{
    visit_mut::VisitMut, ExprCall, ExprMethodCall, FnArg, GenericArgument, GenericParam,
    ImplItemMethod, ItemFn, Lifetime, PredicateLifetime, ReturnType, Signature, TraitItemMethod,
    TypeReference, WhereClause, WherePredicate,
};

////////////////////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////     REPAIR HELPERS     ////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////
pub struct RepairResult {
    pub success: bool,
    pub repair_count: i32,
    pub has_non_elidible_lifetime: bool,
    pub has_struct_lt: bool,
}

pub trait RepairSystem {
    fn name(&self) -> &str;
    fn repair_project(&self, src_path: &str, manifest_path: &str, fn_name: &str) -> RepairResult;
    fn repair_file(&self, file_name: &str, new_file_name: &str) -> RepairResult;
    fn repair_function(&self, file_name: &str, new_file_name: &str, fn_name: &str) -> RepairResult;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RustcError {
    pub rendered: String,
    pub spans: Vec<RustcSpan>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RustcSpan {
    pub file_name: String,
}

pub fn repair_standard_help(stderr: &str, new_file_name: &str) -> bool {
    let deserializer = serde_json::Deserializer::from_str(stderr);
    let stream = deserializer.into_iter::<RustcError>();
    let mut helped = false;
    for item in stream {
        let rendered = match item {
            Ok(i) => i.rendered,
            Err(_) => stderr.to_string(),
        };
        let re = Regex::new(r"help: consider.+\n.*\n(?P<line_number>\d+) \| (?P<replacement>.+)\n")
            .unwrap();
        let help_lines = re.captures_iter(rendered.as_str());

        let file_content: String = fs::read_to_string(&new_file_name).unwrap().parse().unwrap();

        let lines = file_content.split("\n");
        let mut lines_modifiable = Vec::new();
        for (_, line) in lines.enumerate() {
            lines_modifiable.push(line);
        }

        let mut current_line = 0;

        let out_file = fs::File::create(&new_file_name).unwrap();
        let mut writer = BufWriter::new(out_file);
        for captured in help_lines {
            /*
            println!(
                "line: {:?}, fn: {:?} {}",
                &captured["line_number"], &captured["replacement"], current_line,
            );
             */

            let line_number = match captured["line_number"].parse::<usize>() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let replacement = &captured["replacement"];
            if replacement.contains("&'lifetime") {
                continue;
            }

            helped = true;
            while current_line < line_number - 1 {
                writeln!(writer, "{}", lines_modifiable[current_line]).unwrap();
                current_line += 1;
            }
            writeln!(writer, "{}", replacement).unwrap();
            current_line += 1;
        }
        while current_line < lines_modifiable.len() {
            writeln!(writer, "{}", lines_modifiable[current_line]).unwrap();
            current_line += 1;
        }
    }
    helped
}

struct FnLifetimeBounder<'a> {
    fn_name: &'a str,
    lifetime: &'a str,
    bound: &'a str,
    success: bool,
}

impl VisitMut for FnLifetimeBounder<'_> {
    fn visit_impl_item_method_mut(&mut self, i: &mut ImplItemMethod) {
        let id = i.sig.ident.to_string();
        //println!("caller name: {}, at: {}", self.caller_fn_name, &id);
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_bounder(&mut i.sig),
        }
        syn::visit_mut::visit_impl_item_method_mut(self, i);
    }

    fn visit_item_fn_mut(&mut self, i: &mut ItemFn) {
        let id = i.sig.ident.to_string();
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_bounder(&mut i.sig),
        }
    }

    fn visit_trait_item_method_mut(&mut self, i: &mut TraitItemMethod) {
        let id = i.sig.ident.to_string();
        //println!("caller name: {}, at: {}", self.caller_fn_name, &id);
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_bounder(&mut i.sig),
        }
        syn::visit_mut::visit_trait_item_method_mut(self, i);
    }
}

impl FnLifetimeBounder<'_> {
    fn fn_lifetime_bounder(&mut self, sig: &mut Signature) {
        let gen = &mut sig.generics;
        let wc = gen.where_clause.get_or_insert(WhereClause {
            where_token: Default::default(),
            predicates: Default::default(),
        });
        let mut wp = PredicateLifetime {
            lifetime: Lifetime::new(self.lifetime, Span::call_site()),
            colon_token: Default::default(),
            bounds: Default::default(),
        };
        wp.bounds.push(Lifetime::new(self.bound, Span::call_site()));
        wc.predicates.push(WherePredicate::Lifetime(wp));
        self.success = true
    }
}

pub fn repair_bounds_help(stderr: &str, new_file_name: &str, fn_name: &str) -> bool {
    let deserializer = serde_json::Deserializer::from_str(stderr);
    let stream = deserializer.into_iter::<RustcError>();
    let mut helped = false;
    for item in stream {
        let rendered = match item {
            Ok(i) => i.rendered,
            Err(_) => stderr.to_string(),
        };
        let re = Regex::new(r"= help: consider.+bound: `(?P<constraint_lhs>'[a-z0-9]+): (?P<constraint_rhs>'[a-z0-9]+)`").unwrap();
        let help_lines = re.captures_iter(rendered.as_str());
        /*
            &caps["line_number"],
            &caps["fn_sig"],
            &caps["constraint_lhs"],
            &caps["constraint_rhs"],
        */
        for captured in help_lines {
            // println!("found helps: {}, {}",
            //          &captured["constraint_lhs"],
            //          &captured["constraint_rhs"]);
            let file_content: String = fs::read_to_string(&new_file_name).unwrap().parse().unwrap();
            let mut file = syn::parse_str::<syn::File>(file_content.as_str())
                .map_err(|e| format!("{:?}", e))
                .unwrap();
            let mut visit = FnLifetimeBounder {
                fn_name,
                lifetime: &captured["constraint_lhs"],
                bound: &captured["constraint_rhs"],
                success: false,
            };
            visit.visit_file_mut(&mut file);
            let file = file.into_token_stream().to_string();
            match visit.success {
                true => {
                    fs::write(new_file_name.to_string(), format_source(&file)).unwrap();
                    helped = true;
                }
                false => (),
            }
        }
    }
    helped
}

pub fn repair_iteration(
    compile_cmd: &mut Command,
    process_errors: &dyn Fn(&str) -> bool,
    print_stats: bool,
    max_iterations: Option<i32>,
) -> RepairResult {
    let mut count = 0;
    let max_iterations = max_iterations.unwrap_or(25);
    let mut repair_result = RepairResult {
        success: false,
        repair_count: 0,
        has_non_elidible_lifetime: false,
        has_struct_lt: false,
    };

    let success = loop {
        let out = compile_cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            break true;
        }
        count += 1;

        let temp = stderr.to_string();
        if !process_errors(temp.as_str()) {
            break false;
        }
        if max_iterations == count {
            break false;
        }
    };

    if print_stats {
        info!("repair count: {}", count);
        info!("status: {}", success);
    }

    repair_result.success = success;
    repair_result.repair_count = count;
    repair_result
}

////////////////////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////    ELIDING LIFETIMES   ////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////
struct FnLifetimeEliderTypeHelper<'a> {
    cannot_elide: &'a Vec<String>,
    lt_count: &'a HashMap<&'a String, i32>,
}

impl VisitMut for FnLifetimeEliderTypeHelper<'_> {
    // don't elide fully just replace with '_
    // fn visit_angle_bracketed_generic_arguments_mut(
    //     &mut self,
    //     i: &mut AngleBracketedGenericArguments,
    // ) {
    //     i.args = i
    //         .args
    //         .clone()
    //         .into_iter()
    //         .filter(|arg| match arg {
    //             GenericArgument::Lifetime(lt) => {
    //                 let id = lt.to_string();
    //                 if !self.lt_count.contains_key(&id) {
    //                     false
    //                 } else {
    //                     let result =
    //                         self.cannot_elide.contains(&id) || *self.lt_count.get(&id).unwrap() > 1;
    //                     result
    //                 }
    //             }
    //             _ => true,
    //         })
    //         .collect();
    //     syn::visit_mut::visit_angle_bracketed_generic_arguments_mut(self, i);
    // }

    fn visit_type_reference_mut(&mut self, i: &mut TypeReference) {
        match &mut i.lifetime {
            None => (),
            Some(lt) => {
                let id = lt.to_string();
                if !self.cannot_elide.contains(&id)
                    && (!self.lt_count.contains_key(&id) || *self.lt_count.get(&id).unwrap() <= 1)
                {
                    i.lifetime = None
                }
            }
        };
        syn::visit_mut::visit_type_reference_mut(self, i);
    }
}

struct FnLifetimeEliderArgHelper<'a> {
    cannot_elide: &'a Vec<String>,
    lt_count: &'a HashMap<&'a String, i32>,
}

impl VisitMut for FnLifetimeEliderArgHelper<'_> {
    fn visit_fn_arg_mut(&mut self, i: &mut FnArg) {
        match i {
            FnArg::Typed(t) => {
                let mut type_helper = FnLifetimeEliderTypeHelper {
                    cannot_elide: self.cannot_elide,
                    lt_count: self.lt_count,
                };
                type_helper.visit_type_mut(t.ty.as_mut());
            }
            FnArg::Receiver(_) => (), // cannot elide if there is self
        }
    }
}

struct FnLifetimeElider<'a> {
    fn_name: &'a str,
    annotations_left: bool,
    has_struct_lt: bool,
}

struct LtGetterElider<'a> {
    v: &'a mut Vec<String>,
}
impl VisitMut for LtGetterElider<'_> {
    fn visit_lifetime_mut(&mut self, i: &mut Lifetime) {
        let id = i.to_string();
        self.v.push(id.to_string());
        syn::visit_mut::visit_lifetime_mut(self, i)
    }
}

struct ChangeLtHelperElider<'a> {
    map: &'a HashMap<String, String>,
    has_struct_lt: bool,
}

impl VisitMut for ChangeLtHelperElider<'_> {
    fn visit_generic_argument_mut(&mut self, i: &mut GenericArgument) {
        debug!("generic: {:?}", i);
        match i {
            GenericArgument::Lifetime(l) => {
                let id = l.to_string();
                debug!("generic lt: {:?}", &id);
                self.has_struct_lt = true;
                match self.map.get(&id) {
                    Some(new_lt) => *l = Lifetime::new(new_lt.as_str(), Span::call_site()),
                    None => *l = Lifetime::new("'_", Span::call_site()),
                }
            }
            _ => syn::visit_mut::visit_generic_argument_mut(self, i),
        }
    }
    fn visit_lifetime_mut(&mut self, i: &mut Lifetime) {
        let id = i.to_string();
        match self.map.get(&id) {
            Some(new_lt) => *i = Lifetime::new(new_lt.as_str(), Span::call_site()),
            None => (),
        }
        syn::visit_mut::visit_lifetime_mut(self, i)
    }
}

impl VisitMut for FnLifetimeElider<'_> {
    fn visit_impl_item_method_mut(&mut self, i: &mut ImplItemMethod) {
        let id = i.sig.ident.to_string();
        //println!("caller name: {}, at: {}", self.caller_fn_name, &id);
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_elider(&mut i.sig),
        }
        syn::visit_mut::visit_impl_item_method_mut(self, i);
    }

    fn visit_item_fn_mut(&mut self, i: &mut ItemFn) {
        let id = i.sig.ident.to_string();
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_elider(&mut i.sig),
        }
    }

    fn visit_trait_item_method_mut(&mut self, i: &mut TraitItemMethod) {
        let id = i.sig.ident.to_string();
        //println!("caller name: {}, at: {}", self.caller_fn_name, &id);
        match id == self.fn_name.to_string() {
            false => (),
            true => self.fn_lifetime_elider(&mut i.sig),
        }
        syn::visit_mut::visit_trait_item_method_mut(self, i);
    }
}

impl FnLifetimeElider<'_> {
    fn fn_lifetime_elider(&mut self, sig: &mut Signature) {
        // println!("original : {}", i.sig.clone().into_token_stream().to_string());
        let gen = &mut sig.generics;
        let mut cannot_elide = vec![];
        match &gen.where_clause {
            None => (),
            Some(wc) => wc.predicates.iter().for_each(|wp| match wp {
                WherePredicate::Lifetime(lt) => {
                    cannot_elide.push(lt.lifetime.to_string());
                    cannot_elide.push(lt.bounds.first().unwrap().to_string())
                }
                _ => (),
            }),
        }
        match sig.output.borrow_mut() {
            ReturnType::Default => (),
            ReturnType::Type(_, ty) => {
                let mut get_lt = LtGetterElider {
                    v: &mut cannot_elide,
                };
                get_lt.visit_type_mut(ty.clone().as_mut());
            }
        };

        let inputs = &mut sig.inputs;
        let mut has_receiver = false;
        let mut map = HashMap::new();
        let mut v = vec![];
        inputs.iter_mut().for_each(|fn_arg| {
            match fn_arg {
                FnArg::Receiver(_) => has_receiver = true,
                FnArg::Typed(_) => {
                    let mut get_lt = LtGetterElider { v: &mut v };
                    get_lt.visit_fn_arg_mut(fn_arg)
                }
            };
        });
        match has_receiver {
            true => (),
            false => {
                match sig.output.borrow_mut() {
                    ReturnType::Default => (),
                    ReturnType::Type(_, ty) => {
                        let mut get_lt = LtGetterElider { v: &mut v };
                        get_lt.visit_type_mut(ty.clone().as_mut());
                    }
                };
                gen.params.iter_mut().for_each(|gp| match gp {
                    GenericParam::Lifetime(_) => (),
                    gp => {
                        let mut get_lt = LtGetterElider { v: &mut v };
                        get_lt.visit_generic_param_mut(gp);
                    }
                });
                v.iter().for_each(|lt| {
                    match map.contains_key(lt) {
                        true => map.insert(lt, *map.get(lt).unwrap() + 1),
                        false => map.insert(lt, 1),
                    };
                });
                let mut fn_arg_helper = FnLifetimeEliderArgHelper {
                    cannot_elide: &cannot_elide,
                    lt_count: &map,
                };
                inputs
                    .iter_mut()
                    .for_each(|fn_arg| fn_arg_helper.visit_fn_arg_mut(fn_arg));

                match sig.output.borrow_mut() {
                    ReturnType::Default => (),
                    ReturnType::Type(_, ty) => {
                        let mut type_helper = FnLifetimeEliderTypeHelper {
                            cannot_elide: &cannot_elide,
                            lt_count: &map,
                        };
                        type_helper.visit_type_mut(ty.as_mut());
                    }
                };
                gen.params.iter_mut().for_each(|gp| match gp {
                    GenericParam::Lifetime(_) => (),
                    gp => {
                        let mut type_helper = FnLifetimeEliderTypeHelper {
                            cannot_elide: &cannot_elide,
                            lt_count: &map,
                        };
                        type_helper.visit_generic_param_mut(gp);
                    }
                });
                gen.params = gen
                    .params
                    .iter()
                    .cloned()
                    .filter(|g| match g {
                        GenericParam::Lifetime(lt) => {
                            let id = lt.lifetime.to_string();
                            if !map.contains_key(&id) {
                                false
                            } else {
                                let result =
                                    *map.get(&id).unwrap() > 1 || cannot_elide.contains(&id);
                                debug!("lt: {}, result: {}", id, result);
                                result
                            }
                        }
                        _ => true,
                    })
                    .collect();

                let mut lt_count = 0;
                let mut new_lts = HashMap::new();
                gen.params.iter_mut().for_each(|gp| match gp {
                    GenericParam::Lifetime(lt) => {
                        let id = lt.lifetime.to_string();
                        self.annotations_left = true;
                        new_lts.insert(id, format!("'lt{}", lt_count));
                        lt.lifetime =
                            Lifetime::new(format!("'lt{}", lt_count).as_str(), Span::call_site());
                        lt_count += 1
                    }
                    _ => (),
                });
                gen.params.iter_mut().for_each(|gp| match gp {
                    GenericParam::Lifetime(_) => (),
                    gp => {
                        let mut change_lt = ChangeLtHelperElider {
                            map: &new_lts,
                            has_struct_lt: false,
                        };
                        if change_lt.has_struct_lt {
                            self.has_struct_lt = true;
                        }
                        change_lt.visit_generic_param_mut(gp);
                    }
                });
                match &mut gen.where_clause {
                    None => (),
                    Some(wc) => wc.predicates.iter_mut().for_each(|wp| match wp {
                        WherePredicate::Lifetime(lt) => {
                            let id = lt.lifetime.to_string();
                            match new_lts.get(&id) {
                                Some(new_lt) => {
                                    lt.lifetime = Lifetime::new(new_lt.as_str(), Span::call_site())
                                }
                                None => (),
                            };
                            lt.bounds.iter_mut().for_each(|bound| {
                                let id = bound.to_string();
                                match new_lts.get(&id) {
                                    Some(new_lt) => {
                                        *bound = Lifetime::new(new_lt.as_str(), Span::call_site())
                                    }
                                    None => (),
                                }
                            })
                        }
                        _ => (),
                    }),
                }
                inputs.iter_mut().for_each(|fn_arg| match fn_arg {
                    FnArg::Receiver(_) => (),
                    FnArg::Typed(t) => {
                        let mut change_lt = ChangeLtHelperElider {
                            map: &new_lts,
                            has_struct_lt: false,
                        };
                        if change_lt.has_struct_lt {
                            self.has_struct_lt = true;
                        }
                        debug!("debugging input: {:?}", t);
                        change_lt.visit_pat_type_mut(t);
                    }
                });
                match sig.output.borrow_mut() {
                    ReturnType::Default => (),
                    ReturnType::Type(_, ty) => {
                        let mut change_lt = ChangeLtHelperElider {
                            map: &new_lts,
                            has_struct_lt: false,
                        };
                        if change_lt.has_struct_lt {
                            self.has_struct_lt = true;
                        }
                        change_lt.visit_type_mut(ty.as_mut());
                    }
                }
            }
        }
    }
}

pub struct ElideLifetimeResult {
    pub success: bool,
    pub annotations_left: bool,
    pub has_struct_lt: bool,
}

/**
Elide lifetimes that are only used once in the inputs and not used in output(s)/bound(s)

Do not elide lifetimes when receiver (self) is in the input

Elision rules are here: https://doc.rust-lang.org/nomicon/lifetime-elision.htm
*/
pub fn elide_lifetimes_annotations(new_file_name: &str, fn_name: &str) -> ElideLifetimeResult {
    let file_content: String = fs::read_to_string(&new_file_name).unwrap().parse().unwrap();
    let mut file = syn::parse_str::<syn::File>(file_content.as_str())
        .map_err(|e| format!("{:?}", e))
        .unwrap();
    let mut visit = FnLifetimeElider {
        fn_name,
        annotations_left: false,
        has_struct_lt: false,
    };
    visit.visit_file_mut(&mut file);
    let file = file.into_token_stream().to_string();
    fs::write(new_file_name.to_string(), format_source(&file)).unwrap();
    ElideLifetimeResult {
        success: true,
        annotations_left: visit.annotations_left,
        has_struct_lt: visit.has_struct_lt,
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////     CALLEE RENAMER    ////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////
pub struct RenameFn<'a> {
    pub(crate) callee_name: &'a str,
    pub(crate) callee_postfix: &'a str,
}

impl VisitMut for RenameFn<'_> {
    fn visit_expr_method_call_mut(&mut self, i: &mut ExprMethodCall) {
        let callee = i.clone().method.into_token_stream().to_string();
        match callee.contains(self.callee_name) {
            true => {
                i.method = syn::parse_str(callee.replace(self.callee_postfix, "").as_str()).unwrap()
            }
            false => syn::visit_mut::visit_expr_method_call_mut(self, i),
        }
    }

    fn visit_expr_call_mut(&mut self, i: &mut ExprCall) {
        let callee = i.func.as_ref().into_token_stream().to_string();
        match callee.contains(self.callee_name) {
            true => {
                debug!("callee: {} matched", &callee);
                *i.func.as_mut() =
                    syn::parse_str(callee.replace(self.callee_postfix, "").as_str()).unwrap();
            }
            false => {}
        }
        syn::visit_mut::visit_expr_call_mut(self, i);
    }
    fn visit_impl_item_method_mut(&mut self, i: &mut ImplItemMethod) {
        let callee = i.sig.ident.to_string();
        match callee.contains(self.callee_name) {
            true => {
                i.sig.ident =
                    syn::parse_str(callee.replace(self.callee_postfix, "").as_str()).unwrap();
            }
            false => {}
        }
        syn::visit_mut::visit_impl_item_method_mut(self, i);
    }

    fn visit_item_fn_mut(&mut self, i: &mut ItemFn) {
        let callee = i.sig.ident.to_string();
        match callee.contains(self.callee_name) {
            true => {
                i.sig.ident =
                    syn::parse_str(callee.replace(self.callee_postfix, "").as_str()).unwrap();
            }
            false => {}
        }
        syn::visit_mut::visit_item_fn_mut(self, i);
    }
    fn visit_trait_item_method_mut(&mut self, i: &mut TraitItemMethod) {
        let callee = i.sig.ident.to_string();
        match callee.contains(self.callee_name) {
            true => {
                i.sig.ident =
                    syn::parse_str(callee.replace(self.callee_postfix, "").as_str()).unwrap();
            }
            false => {}
        }
        syn::visit_mut::visit_trait_item_method_mut(self, i);
    }
}

pub fn callee_renamer(new_file_name: &str, fn_name: &str) {
    let file_content: String = fs::read_to_string(&new_file_name).unwrap().parse().unwrap();
    let mut file = syn::parse_str::<syn::File>(file_content.as_str())
        .map_err(|e| format!("{:?}", e))
        .unwrap();
    let mut visitor = RenameFn {
        callee_name: fn_name,
        callee_postfix: "____EXTRACT_THIS",
    };
    visitor.visit_file_mut(&mut file);
    let file = file.into_token_stream().to_string();
    fs::write(new_file_name.to_string(), format_source(&file)).unwrap()
}

////////////////////////////////////////////////////////////////////////////////////////////////////
////////////////////////////////     PROJECT HELPERS    ////////////////////////////////////////////
////////////////////////////////////////////////////////////////////////////////////////////////////
#[derive(Serialize, Deserialize, Debug)]
pub struct CargoError {
    pub message: Option<RustcError>,
}

pub fn repair_iteration_project(
    compile_cmd: &mut Command,
    src_path: &str,
    process_errors: &dyn Fn(&RustcError) -> bool,
    print_stats: bool,
    max_iterations: Option<i32>,
) -> RepairResult {
    let mut count = 0;
    let max_iterations = max_iterations.unwrap_or(25);
    let mut repair_result = RepairResult {
        success: false,
        repair_count: 0,
        has_non_elidible_lifetime: false,
        has_struct_lt: false,
    };
    let success = loop {
        let out = compile_cmd.output().unwrap();
        if out.status.success() {
            info!("repair succeeded");
            break true;
        }
        // cargo give rustc error to stdout not stderr
        let stdout = String::from_utf8_lossy(&out.stdout);
        let binding = stdout.to_string();
        let deserializer = serde_json::Deserializer::from_str(binding.as_str());
        let stream = deserializer.into_iter::<CargoError>();
        count += 1;

        let mut help = false;
        let mut last_failure = format!("");
        for item in stream {
            match &item {
                Ok(item) => match &item.message {
                    None => {}
                    Some(message) => {
                        let spans = &message.spans;
                        debug!("message: {:?}", &message);
                        for span in spans {
                            if src_path.contains(&span.file_name) {
                                debug!("processing error: {}", &message.rendered);
                                last_failure = message.rendered.clone();
                                if process_errors(&message) {
                                    help = true;
                                    break;
                                }
                            }
                        }
                    }
                },
                Err(e) => {
                    debug!("error parsing cargo error:\n{}", e);
                }
            }
        }

        if !help {
            debug!("last failure:\n{}", last_failure);
            break false;
        }

        if max_iterations == count {
            debug!("last failure:\n{}", last_failure);
            break false;
        }
    };

    if print_stats {
        info!("repair count: {}", count);
        info!("status: {}", success);
    }

    repair_result.success = success;
    repair_result.repair_count = count;
    repair_result
}
