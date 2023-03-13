use std::convert::{TryFrom, TryInto};

use hex::encode as hex_encode;
use next_binding::swc::core::{
    common::{
        comments::{Comment, CommentKind, Comments},
        errors::HANDLER,
        util::take::Take,
        BytePos, FileName, DUMMY_SP,
    },
    ecma::{
        ast::*,
        atoms::JsWord,
        utils::{private_ident, quote_ident, ExprFactory},
        visit::{as_folder, noop_visit_mut_type, Fold, VisitMut, VisitMutWith},
    },
};
use serde::Deserialize;
use sha1::{Digest, Sha1};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub is_server: bool,
}

pub fn server_actions<C: Comments>(
    file_name: &FileName,
    config: Config,
    comments: C,
) -> impl VisitMut + Fold {
    as_folder(ServerActions {
        config,
        comments,
        file_name: file_name.clone(),
        start_pos: BytePos(0),
        in_action_file: false,
        in_export_decl: false,
        in_default_export_decl: false,
        has_action: false,
        top_level: false,

        ident_cnt: 0,
        in_module: true,
        in_action_fn: false,
        in_action_closure: false,
        closure_idents: Default::default(),
        action_idents: Default::default(),
        exported_idents: Default::default(),

        annotations: Default::default(),
        extra_items: Default::default(),
        export_actions: Default::default(),
    })
}

struct ServerActions<C: Comments> {
    #[allow(unused)]
    config: Config,
    file_name: FileName,
    comments: C,

    start_pos: BytePos,
    in_action_file: bool,
    in_export_decl: bool,
    in_default_export_decl: bool,
    has_action: bool,
    top_level: bool,

    ident_cnt: u32,
    in_module: bool,
    in_action_fn: bool,
    in_action_closure: bool,
    closure_idents: Vec<Id>,
    action_idents: Vec<Name>,

    // (ident, export name)
    exported_idents: Vec<(Id, String)>,

    annotations: Vec<Stmt>,
    extra_items: Vec<ModuleItem>,
    export_actions: Vec<String>,
}

impl<C: Comments> ServerActions<C> {
    // Check if the function or arrow function is an action function
    fn get_action_info(
        &mut self,
        maybe_body: Option<&mut BlockStmt>,
        remove_directive: bool,
    ) -> bool {
        let mut is_action_fn = false;

        if self.in_action_file && self.in_export_decl {
            // All export functions in a server file are actions
            is_action_fn = true;
        } else {
            // Check if the function has `"use server"`
            if let Some(body) = maybe_body {
                let directive_index = get_server_directive_index_in_fn(&body.stmts);
                if directive_index >= 0 {
                    is_action_fn = true;
                    if remove_directive {
                        body.stmts.remove(directive_index.try_into().unwrap());
                    }
                }
            }
        }

        is_action_fn
    }

    fn add_action_annotations_and_maybe_hoist(
        &mut self,
        ident: &Ident,
        function: Option<&mut Box<Function>>,
        arrow: Option<&mut ArrowExpr>,
    ) -> (Option<Box<Function>>, Option<Box<ArrowExpr>>) {
        let action_name: JsWord = gen_ident(&mut self.ident_cnt);
        let action_ident = private_ident!(action_name.clone());

        let export_name: JsWord = if self.in_default_export_decl {
            "default".into()
        } else {
            action_name
        };

        self.has_action = true;
        self.export_actions.push(export_name.to_string());

        // If it's already a top level function, we don't need to hoist it.
        if self.top_level && arrow.is_none() {
            annotate_ident_as_action(
                &mut self.annotations,
                ident.clone(),
                Vec::new(),
                self.file_name.to_string(),
                export_name.to_string(),
            );

            // export const $ACTION_myAction = myAction;
            self.extra_items
                .push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                    span: DUMMY_SP,
                    decl: Decl::Var(Box::new(VarDecl {
                        span: DUMMY_SP,
                        kind: VarDeclKind::Const,
                        declare: Default::default(),
                        decls: vec![VarDeclarator {
                            span: DUMMY_SP,
                            name: action_ident.into(),
                            init: Some(ident.clone().into()),
                            definite: Default::default(),
                        }],
                    })),
                })));
        } else {
            // Hoist the function to the top level and export it. To hoist it, we need to
            // first Collect all the identifiers defined in the closure and used
            // in the action function. Dedup the identifiers.
            let mut added_ids = Vec::new();
            let mut ids_from_closure = self.action_idents.clone();
            ids_from_closure.retain(|id| {
                if added_ids.contains(id) {
                    false
                } else if self.closure_idents.contains(&id.0) {
                    added_ids.push(id.clone());
                    true
                } else {
                    false
                }
            });

            let closure_arg = private_ident!("closure");

            let call = CallExpr {
                span: DUMMY_SP,
                callee: action_ident.clone().as_callee(),
                args: vec![ident.clone().make_member(quote_ident!("$$bound")).as_arg()],
                type_args: Default::default(),
            };

            annotate_ident_as_action(
                &mut self.annotations,
                ident.clone(),
                ids_from_closure
                    .iter()
                    .cloned()
                    .map(|id| Some(id.as_arg()))
                    .collect(),
                self.file_name.to_string(),
                export_name.to_string(),
            );

            if let Some(a) = arrow {
                if let BlockStmtOrExpr::BlockStmt(block) = &mut a.body {
                    block.visit_mut_with(&mut ClosureReplacer {
                        closure_arg: &closure_arg,
                        used_ids: &ids_from_closure,
                    });
                }

                let new_arrow = ArrowExpr {
                    span: DUMMY_SP,
                    params: a.params.clone(),
                    body: BlockStmtOrExpr::Expr(Box::new(Expr::Call(call))),
                    is_async: a.is_async,
                    is_generator: a.is_generator,
                    type_params: Default::default(),
                    return_type: Default::default(),
                };

                // export const $ACTION_myAction = async () => {}
                self.extra_items
                    .push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                        span: DUMMY_SP,
                        decl: Decl::Var(Box::new(VarDecl {
                            span: DUMMY_SP,
                            kind: VarDeclKind::Const,
                            declare: Default::default(),
                            decls: vec![VarDeclarator {
                                span: DUMMY_SP,
                                name: action_ident.into(),
                                init: Some(Box::new(Expr::Arrow(ArrowExpr {
                                    params: vec![closure_arg.into()],
                                    ..a.clone()
                                }))),
                                definite: Default::default(),
                            }],
                        })),
                    })));

                return (None, Some(Box::new(new_arrow)));
            } else if let Some(f) = function {
                f.body.visit_mut_with(&mut ClosureReplacer {
                    closure_arg: &closure_arg,
                    used_ids: &ids_from_closure,
                });

                let new_fn = Function {
                    params: f.params.clone(),
                    decorators: f.decorators.take(),
                    span: f.span,
                    body: Some(BlockStmt {
                        span: DUMMY_SP,
                        stmts: vec![Stmt::Return(ReturnStmt {
                            span: DUMMY_SP,
                            arg: Some(call.into()),
                        })],
                    }),
                    is_generator: f.is_generator,
                    is_async: f.is_async,
                    type_params: Default::default(),
                    return_type: Default::default(),
                };

                // export async function $ACTION_myAction () {}
                self.extra_items
                    .push(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl {
                        span: DUMMY_SP,
                        decl: FnDecl {
                            ident: action_ident,
                            function: Box::new(Function {
                                params: vec![closure_arg.into()],
                                ..*f.take()
                            }),
                            declare: Default::default(),
                        }
                        .into(),
                    })));

                return (Some(Box::new(new_fn)), None);
            }
        }

        (None, None)
    }
}

impl<C: Comments> VisitMut for ServerActions<C> {
    fn visit_mut_export_decl(&mut self, decl: &mut ExportDecl) {
        let old = self.in_export_decl;
        self.in_export_decl = true;
        decl.decl.visit_mut_with(self);
        self.in_export_decl = old;
    }

    fn visit_mut_export_default_decl(&mut self, decl: &mut ExportDefaultDecl) {
        let old = self.in_export_decl;
        let old_default = self.in_default_export_decl;
        self.in_export_decl = true;
        self.in_default_export_decl = true;
        decl.decl.visit_mut_with(self);
        self.in_export_decl = old;
        self.in_default_export_decl = old_default;
    }

    fn visit_mut_export_default_expr(&mut self, expr: &mut ExportDefaultExpr) {
        let old = self.in_export_decl;
        let old_default = self.in_default_export_decl;
        self.in_export_decl = true;
        self.in_default_export_decl = true;
        expr.expr.visit_mut_with(self);
        self.in_export_decl = old;
        self.in_default_export_decl = old_default;
    }

    fn visit_mut_fn_expr(&mut self, f: &mut FnExpr) {
        let is_action_fn = self.get_action_info(f.function.body.as_mut(), true);

        {
            // Visit children
            let old_in_action_fn = self.in_action_fn;
            let old_in_module = self.in_module;
            let old_in_action_closure = self.in_action_closure;
            let old_in_export_decl = self.in_export_decl;
            let old_in_default_export_decl = self.in_default_export_decl;
            self.in_action_fn = is_action_fn;
            self.in_module = false;
            self.in_action_closure = true;
            self.in_export_decl = false;
            self.in_default_export_decl = false;
            f.visit_mut_children_with(self);
            self.in_action_fn = old_in_action_fn;
            self.in_module = old_in_module;
            self.in_action_closure = old_in_action_closure;
            self.in_export_decl = old_in_export_decl;
            self.in_default_export_decl = old_in_default_export_decl;
        }

        if !is_action_fn {
            return;
        }

        if !f.function.is_async {
            HANDLER.with(|handler| {
                handler
                    .struct_span_err(f.function.span, "Server actions must be async functions")
                    .emit();
            });
        } else if !self.in_action_file {
            if f.ident.is_none() {
                let action_name = gen_ident(&mut self.ident_cnt);
                f.ident = Some(Ident::new(action_name, DUMMY_SP));
            }

            let (maybe_new_fn, _) = self.add_action_annotations_and_maybe_hoist(
                f.ident.as_mut().unwrap(),
                Some(&mut f.function),
                None,
            );

            if let Some(new_fn) = maybe_new_fn {
                f.function = new_fn;
            }
        }
    }

    fn visit_mut_fn_decl(&mut self, f: &mut FnDecl) {
        let is_action_fn = self.get_action_info(f.function.body.as_mut(), true);

        {
            // Visit children
            let old_in_action_fn = self.in_action_fn;
            let old_in_module = self.in_module;
            let old_in_action_closure = self.in_action_closure;
            let old_in_export_decl = self.in_export_decl;
            let old_in_default_export_decl = self.in_default_export_decl;
            self.in_action_fn = is_action_fn;
            self.in_module = false;
            self.in_action_closure = true;
            self.in_export_decl = false;
            self.in_default_export_decl = false;
            f.visit_mut_children_with(self);
            self.in_action_fn = old_in_action_fn;
            self.in_module = old_in_module;
            self.in_action_closure = old_in_action_closure;
            self.in_export_decl = old_in_export_decl;
            self.in_default_export_decl = old_in_default_export_decl;
        }

        if !is_action_fn {
            return;
        }

        if !f.function.is_async {
            HANDLER.with(|handler| {
                handler
                    .struct_span_err(f.ident.span, "Server actions must be async functions")
                    .emit();
            });
        } else if !self.in_action_file {
            let (maybe_new_fn, _) =
                self.add_action_annotations_and_maybe_hoist(&f.ident, Some(&mut f.function), None);

            if let Some(new_fn) = maybe_new_fn {
                f.function = new_fn;
            }
        }
    }

    fn visit_mut_arrow_expr(&mut self, a: &mut ArrowExpr) {
        // Arrow expressions need to be visited in prepass to determine if it's
        // an action function or not.
        let is_action_fn = self.get_action_info(
            if let BlockStmtOrExpr::BlockStmt(block) = &mut a.body {
                Some(block)
            } else {
                None
            },
            false,
        );

        {
            // Visit children
            let old_in_action_fn = self.in_action_fn;
            let old_in_module = self.in_module;
            let old_in_action_closure = self.in_action_closure;
            let old_in_export_decl = self.in_export_decl;
            let old_in_default_export_decl = self.in_default_export_decl;
            self.in_action_fn = is_action_fn;
            self.in_module = false;
            self.in_action_closure = true;
            self.in_export_decl = false;
            self.in_default_export_decl = false;
            a.visit_mut_children_with(self);
            self.in_action_fn = old_in_action_fn;
            self.in_module = old_in_module;
            self.in_action_closure = old_in_action_closure;
            self.in_export_decl = old_in_export_decl;
            self.in_default_export_decl = old_in_default_export_decl;
        }

        if !is_action_fn {
            return;
        }

        if !a.is_async && !self.in_action_file {
            HANDLER.with(|handler| {
                handler
                    .struct_span_err(a.span, "Server actions must be async functions")
                    .emit();
            });
        }
    }

    fn visit_mut_module(&mut self, m: &mut Module) {
        self.start_pos = m.span.lo;
        m.visit_mut_children_with(self);
    }

    fn visit_mut_stmt(&mut self, n: &mut Stmt) {
        n.visit_mut_children_with(self);

        if self.in_module {
            return;
        }

        let ids = collect_idents_in_stmt(n);
        if !self.in_action_fn && !self.in_action_file {
            self.closure_idents.extend(ids);
        }
    }

    fn visit_mut_param(&mut self, n: &mut Param) {
        n.visit_mut_children_with(self);

        if !self.in_action_fn && !self.in_action_file {
            match &n.pat {
                Pat::Ident(ident) => {
                    self.closure_idents.push(ident.id.to_id());
                }
                Pat::Array(array) => {
                    self.closure_idents
                        .extend(collect_idents_in_array_pat(&array.elems));
                }
                Pat::Object(object) => {
                    self.closure_idents
                        .extend(collect_idents_in_object_pat(&object.props));
                }
                Pat::Rest(rest) => {
                    if let Pat::Ident(ident) = &*rest.arg {
                        self.closure_idents.push(ident.id.to_id());
                    }
                }
                _ => {}
            }
        }
    }

    fn visit_mut_expr(&mut self, n: &mut Expr) {
        if self.in_action_fn && self.in_action_closure {
            if let Ok(name) = Name::try_from(&*n) {
                self.in_action_closure = false;
                self.action_idents.push(name);
                n.visit_mut_children_with(self);
                self.in_action_closure = true;
                return;
            }
        }

        n.visit_mut_children_with(self);

        if !self.in_action_file {
            if let Expr::Arrow(a) = n {
                let is_action_fn = self.get_action_info(
                    if let BlockStmtOrExpr::BlockStmt(block) = &mut a.body {
                        Some(block)
                    } else {
                        None
                    },
                    true,
                );

                if is_action_fn {
                    // We need to give a name to the arrow function
                    // action and hoist it to the top.
                    let action_name = gen_ident(&mut self.ident_cnt);
                    let ident = private_ident!(action_name);

                    let (_, maybe_new_arrow) =
                        self.add_action_annotations_and_maybe_hoist(&ident, None, Some(a));

                    *n = attach_name_to_arrow(
                        ident,
                        if let Some(new_arrow) = maybe_new_arrow {
                            *new_arrow
                        } else {
                            a.clone()
                        },
                        &mut self.extra_items,
                    );
                }
            }
        }
    }

    fn visit_mut_module_items(&mut self, stmts: &mut Vec<ModuleItem>) {
        let directive_index = get_server_directive_index_in_module(stmts);
        if directive_index >= 0 {
            self.in_action_file = true;
            self.has_action = true;
            stmts.remove(directive_index.try_into().unwrap());
        }

        let old_annotations = self.annotations.take();
        let mut new = Vec::with_capacity(stmts.len());

        for mut stmt in stmts.take() {
            self.top_level = true;

            // For action file, it's not allowed to export things other than async
            // functions.
            if self.in_action_file {
                let mut disallowed_export_span = DUMMY_SP;

                // Currrently only function exports are allowed.
                match &mut stmt {
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(ExportDecl { decl, span })) => {
                        match decl {
                            Decl::Fn(f) => {
                                // export function foo() {}
                                self.exported_idents
                                    .push((f.ident.to_id(), f.ident.sym.to_string()));
                            }
                            Decl::Var(var) => {
                                // export const foo = 1
                                let ids: Vec<Id> = collect_idents_in_var_decls(&var.decls);
                                self.exported_idents.extend(
                                    ids.into_iter().map(|id| (id.clone(), id.0.to_string())),
                                );

                                for decl in &mut var.decls {
                                    if let Some(init) = &decl.init {
                                        match &**init {
                                            Expr::Fn(_f) => {}
                                            Expr::Arrow(_a) => {}
                                            _ => {
                                                disallowed_export_span = *span;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {
                                disallowed_export_span = *span;
                            }
                        }
                    }
                    ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(named)) => {
                        if named.src.is_some() {
                            disallowed_export_span = named.span;
                        } else {
                            for spec in &mut named.specifiers {
                                if let ExportSpecifier::Named(ExportNamedSpecifier {
                                    orig: ModuleExportName::Ident(ident),
                                    exported,
                                    ..
                                }) = spec
                                {
                                    if let Some(export_name) = exported {
                                        if let ModuleExportName::Ident(Ident { sym, .. }) =
                                            export_name
                                        {
                                            // export { foo as bar }
                                            self.exported_idents
                                                .push((ident.to_id(), sym.to_string()));
                                        } else if let ModuleExportName::Str(str) = export_name {
                                            // export { foo as "bar" }
                                            self.exported_idents
                                                .push((ident.to_id(), str.value.to_string()));
                                        }
                                    } else {
                                        // export { foo }
                                        self.exported_idents
                                            .push((ident.to_id(), ident.sym.to_string()));
                                    }
                                } else {
                                    disallowed_export_span = named.span;
                                }
                            }
                        }
                    }
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(ExportDefaultDecl {
                        decl,
                        span,
                        ..
                    })) => match decl {
                        DefaultDecl::Fn(f) => {
                            if let Some(ident) = &f.ident {
                                // export default function foo() {}
                                self.exported_idents.push((ident.to_id(), "default".into()));
                            } else {
                                // export default function() {}
                                let new_ident =
                                    Ident::new(gen_ident(&mut self.ident_cnt), DUMMY_SP);
                                f.ident = Some(new_ident.clone());
                                self.exported_idents
                                    .push((new_ident.to_id(), "default".into()));
                            }
                        }
                        _ => {
                            disallowed_export_span = *span;
                        }
                    },
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(default_expr)) => {
                        match &mut *default_expr.expr {
                            Expr::Fn(_f) => {}
                            Expr::Arrow(arrow) => {
                                if !arrow.is_async {
                                    disallowed_export_span = default_expr.span;
                                } else {
                                    // export default async () => {}
                                    let new_ident =
                                        Ident::new(gen_ident(&mut self.ident_cnt), DUMMY_SP);

                                    self.exported_idents
                                        .push((new_ident.to_id(), "default".into()));

                                    *default_expr.expr = attach_name_to_arrow(
                                        new_ident,
                                        arrow.clone(),
                                        &mut self.extra_items,
                                    );
                                }
                            }
                            Expr::Ident(ident) => {
                                // export default foo
                                self.exported_idents.push((ident.to_id(), "default".into()));
                            }
                            _ => {
                                disallowed_export_span = default_expr.span;
                            }
                        }
                    }
                    ModuleItem::ModuleDecl(ModuleDecl::ExportAll(ExportAll { span, .. })) => {
                        disallowed_export_span = *span;
                    }
                    _ => {}
                }

                if disallowed_export_span != DUMMY_SP {
                    HANDLER.with(|handler| {
                        handler
                            .struct_span_err(
                                disallowed_export_span,
                                "Only async functions are allowed to be exported in a \"use \
                                 server\" file.",
                            )
                            .emit();
                    });
                }
            }

            stmt.visit_mut_with(self);

            new.push(stmt);
            new.extend(self.annotations.drain(..).map(ModuleItem::Stmt));
            new.append(&mut self.extra_items);
        }

        // If it's a "use server" file, all exports need to be annotated as actions.
        if self.in_action_file {
            for (id, export_name) in self.exported_idents.iter() {
                let ident = Ident::new(id.0.clone(), DUMMY_SP.with_ctxt(id.1));
                annotate_ident_as_action(
                    &mut self.annotations,
                    ident,
                    Vec::new(),
                    self.file_name.to_string(),
                    export_name.to_string(),
                );
            }
            new.extend(self.annotations.drain(..).map(ModuleItem::Stmt));
            new.append(&mut self.extra_items);
        }

        *stmts = new;

        self.annotations = old_annotations;

        if self.has_action {
            // Prepend a special comment to the top of the file.
            self.comments.add_leading(
                self.start_pos,
                Comment {
                    span: DUMMY_SP,
                    kind: CommentKind::Block,
                    // Append a list of exported actions.
                    text: format!(
                        " __next_internal_action_entry_do_not_use__ {} ",
                        if self.in_action_file {
                            self.exported_idents
                                .iter()
                                .map(|e| e.1.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        } else {
                            self.export_actions.join(",")
                        }
                    )
                    .into(),
                },
            );
        }
    }

    fn visit_mut_stmts(&mut self, stmts: &mut Vec<Stmt>) {
        let old_top_level = self.top_level;
        let old_annotations = self.annotations.take();

        let mut new = Vec::with_capacity(stmts.len());
        for mut stmt in stmts.take() {
            self.top_level = false;
            stmt.visit_mut_with(self);

            new.push(stmt);
            new.append(&mut self.annotations);
        }

        *stmts = new;

        self.annotations = old_annotations;
        self.top_level = old_top_level;
    }

    noop_visit_mut_type!();
}

fn annotate(fn_name: &Ident, field_name: &str, value: Box<Expr>) -> Stmt {
    Stmt::Expr(ExprStmt {
        span: DUMMY_SP,
        expr: AssignExpr {
            span: DUMMY_SP,
            op: op!("="),
            left: PatOrExpr::Expr(fn_name.clone().make_member(quote_ident!(field_name)).into()),
            right: value,
        }
        .into(),
    })
}

fn gen_ident(cnt: &mut u32) -> JsWord {
    let id: JsWord = format!("$$ACTION_{}", cnt).into();
    *cnt += 1;
    id
}

fn attach_name_to_arrow(ident: Ident, arrow: ArrowExpr, extra_items: &mut Vec<ModuleItem>) -> Expr {
    // Create the variable `var $$ACTION_0;`
    extra_items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        kind: VarDeclKind::Var,
        declare: Default::default(),
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: ident.clone().into(),
            init: None,
            definite: Default::default(),
        }],
    })))));

    // Create the assignment `($$ACTION_0 = arrow)`
    Expr::Paren(ParenExpr {
        span: DUMMY_SP,
        expr: Box::new(Expr::Assign(AssignExpr {
            span: DUMMY_SP,
            left: PatOrExpr::Pat(Box::new(Pat::Ident(ident.into()))),
            op: op!("="),
            right: Box::new(Expr::Arrow(arrow)),
        })),
    })
}

fn annotate_ident_as_action(
    annotations: &mut Vec<Stmt>,
    ident: Ident,
    bound: Vec<Option<ExprOrSpread>>,
    file_name: String,
    export_name: String,
) {
    // myAction.$$typeof = Symbol.for('react.server.reference');
    annotations.push(annotate(
        &ident,
        "$$typeof",
        CallExpr {
            span: DUMMY_SP,
            callee: quote_ident!("Symbol")
                .make_member(quote_ident!("for"))
                .as_callee(),
            args: vec!["react.server.reference".as_arg()],
            type_args: Default::default(),
        }
        .into(),
    ));

    // Attach a checksum to the action using sha1:
    // myAction.$$id = sha1('file_name' + ':' + 'export_name');
    let mut hasher = Sha1::new();
    hasher.update(file_name.as_bytes());
    hasher.update(b":");
    hasher.update(export_name.as_bytes());
    let result = hasher.finalize();

    // Convert result to hex string
    annotations.push(annotate(&ident, "$$id", hex_encode(result).into()));

    // myAction.$$bound = [];
    annotations.push(annotate(
        &ident,
        "$$bound",
        ArrayLit {
            span: DUMMY_SP,
            elems: bound,
        }
        .into(),
    ));
}

fn get_server_directive_index_in_module(stmts: &[ModuleItem]) -> i32 {
    for (i, stmt) in stmts.iter().enumerate() {
        if let ModuleItem::Stmt(Stmt::Expr(first)) = stmt {
            match &*first.expr {
                Expr::Lit(Lit::Str(Str { value, .. })) => {
                    if value == "use server" {
                        return i as i32;
                    }
                }
                _ => return -1,
            }
        } else {
            return -1;
        }
    }
    -1
}

fn get_server_directive_index_in_fn(stmts: &[Stmt]) -> i32 {
    for (i, stmt) in stmts.iter().enumerate() {
        if let Stmt::Expr(first) = stmt {
            match &*first.expr {
                Expr::Lit(Lit::Str(Str { value, .. })) => {
                    if value == "use server" {
                        return i as i32;
                    }
                }
                _ => return -1,
            }
        } else {
            return -1;
        }
    }
    -1
}

fn collect_idents_in_array_pat(elems: &[Option<Pat>]) -> Vec<Id> {
    let mut ids = Vec::new();

    for elem in elems.iter().flatten() {
        match elem {
            Pat::Ident(ident) => {
                ids.push(ident.id.to_id());
            }
            Pat::Array(array) => {
                ids.extend(collect_idents_in_array_pat(&array.elems));
            }
            Pat::Object(object) => {
                ids.extend(collect_idents_in_object_pat(&object.props));
            }
            Pat::Rest(rest) => {
                if let Pat::Ident(ident) = &*rest.arg {
                    ids.push(ident.id.to_id());
                }
            }
            _ => {}
        }
    }

    ids
}

fn collect_idents_in_object_pat(props: &[ObjectPatProp]) -> Vec<Id> {
    let mut ids = Vec::new();

    for prop in props {
        match prop {
            ObjectPatProp::KeyValue(KeyValuePatProp { key, value }) => {
                if let PropName::Ident(ident) = key {
                    ids.push(ident.to_id());
                }

                match &**value {
                    Pat::Ident(ident) => {
                        ids.push(ident.id.to_id());
                    }
                    Pat::Array(array) => {
                        ids.extend(collect_idents_in_array_pat(&array.elems));
                    }
                    Pat::Object(object) => {
                        ids.extend(collect_idents_in_object_pat(&object.props));
                    }
                    _ => {}
                }
            }
            ObjectPatProp::Assign(AssignPatProp { key, .. }) => {
                ids.push(key.to_id());
            }
            ObjectPatProp::Rest(RestPat { arg, .. }) => {
                if let Pat::Ident(ident) = &**arg {
                    ids.push(ident.id.to_id());
                }
            }
        }
    }

    ids
}

fn collect_idents_in_var_decls(decls: &[VarDeclarator]) -> Vec<Id> {
    let mut ids = Vec::new();

    for decl in decls {
        match &decl.name {
            Pat::Ident(ident) => {
                ids.push(ident.id.to_id());
            }
            Pat::Array(array) => {
                ids.extend(collect_idents_in_array_pat(&array.elems));
            }
            Pat::Object(object) => {
                ids.extend(collect_idents_in_object_pat(&object.props));
            }
            _ => {}
        }
    }

    ids
}

fn collect_idents_in_stmt(stmt: &Stmt) -> Vec<Id> {
    let mut ids = Vec::new();

    if let Stmt::Decl(Decl::Var(var)) = &stmt {
        ids.extend(collect_idents_in_var_decls(&var.decls));
    }

    ids
}

pub(crate) struct ClosureReplacer<'a> {
    closure_arg: &'a Ident,
    used_ids: &'a [Name],
}

impl ClosureReplacer<'_> {
    fn index_of_id(&self, i: &Ident) -> Option<usize> {
        let name = Name(i.to_id(), vec![]);
        self.used_ids.iter().position(|used_id| *used_id == name)
    }

    fn index(&self, e: &Expr) -> Option<usize> {
        let name = Name::try_from(e).ok()?;
        self.used_ids.iter().position(|used_id| *used_id == name)
    }
}

impl VisitMut for ClosureReplacer<'_> {
    fn visit_mut_expr(&mut self, e: &mut Expr) {
        e.visit_mut_children_with(self);

        if let Some(index) = self.index(e) {
            *e = Expr::Member(MemberExpr {
                span: DUMMY_SP,
                obj: self.closure_arg.clone().into(),
                prop: MemberProp::Computed(ComputedPropName {
                    span: DUMMY_SP,
                    expr: index.into(),
                }),
            });
        }
    }

    fn visit_mut_prop(&mut self, p: &mut Prop) {
        p.visit_mut_children_with(self);

        if let Prop::Shorthand(i) = p {
            if let Some(index) = self.index_of_id(i) {
                *p = Prop::KeyValue(KeyValueProp {
                    key: PropName::Ident(i.clone()),
                    value: MemberExpr {
                        span: DUMMY_SP,
                        obj: self.closure_arg.clone().into(),
                        prop: MemberProp::Computed(ComputedPropName {
                            span: DUMMY_SP,
                            expr: index.into(),
                        }),
                    }
                    .into(),
                });
            }
        }
    }

    noop_visit_mut_type!();
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Name(Id, Vec<(JsWord, bool)>);

impl TryFrom<&'_ Expr> for Name {
    type Error = ();

    fn try_from(value: &Expr) -> Result<Self, Self::Error> {
        match value {
            Expr::Ident(i) => Ok(Name(i.to_id(), vec![])),
            Expr::Member(e) => e.try_into(),
            Expr::OptChain(e) => e.try_into(),
            _ => Err(()),
        }
    }
}

impl TryFrom<&'_ MemberExpr> for Name {
    type Error = ();

    fn try_from(value: &MemberExpr) -> Result<Self, Self::Error> {
        match &value.prop {
            MemberProp::Ident(prop) => {
                let mut obj: Name = value.obj.as_ref().try_into()?;
                obj.1.push((prop.sym.clone(), true));
                Ok(obj)
            }
            _ => Err(()),
        }
    }
}

impl TryFrom<&'_ OptChainExpr> for Name {
    type Error = ();

    fn try_from(value: &OptChainExpr) -> Result<Self, Self::Error> {
        match &value.base {
            OptChainBase::Member(value) => match &value.prop {
                MemberProp::Ident(prop) => {
                    let mut obj: Name = value.obj.as_ref().try_into()?;
                    obj.1.push((prop.sym.clone(), false));
                    Ok(obj)
                }
                _ => Err(()),
            },
            OptChainBase::Call(_) => Err(()),
        }
    }
}

impl From<Name> for Expr {
    fn from(value: Name) -> Self {
        let mut expr = Expr::Ident(value.0.into());

        for (prop, is_member) in value.1.into_iter() {
            if is_member {
                expr = Expr::Member(MemberExpr {
                    span: DUMMY_SP,
                    obj: expr.into(),
                    prop: MemberProp::Ident(Ident::new(prop, DUMMY_SP)),
                });
            } else {
                expr = Expr::OptChain(OptChainExpr {
                    span: DUMMY_SP,
                    question_dot_token: DUMMY_SP,
                    base: OptChainBase::Member(MemberExpr {
                        span: DUMMY_SP,
                        obj: expr.into(),
                        prop: MemberProp::Ident(Ident::new(prop, DUMMY_SP)),
                    }),
                });
            }
        }

        expr
    }
}
