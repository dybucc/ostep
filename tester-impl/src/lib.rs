#![feature(bool_to_result)]

use std::iter;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::{
    Arm, Block, Expr, ExprArray, ExprAssign, ExprBinary, ExprBlock, ExprBreak, ExprCall, ExprCast,
    ExprField, ExprForLoop, ExprGroup, ExprIf, ExprIndex, ExprLet, ExprLoop, ExprMatch,
    ExprMethodCall, ExprParen, ExprRawAddr, ExprReference, ExprRepeat, ExprReturn, ExprStruct,
    ExprTry, ExprTuple, ExprUnary, ExprUnsafe, ExprWhile, ItemFn, Local, LocalInit, PatConst,
    PatRange, Path, PathSegment, ReturnType, Signature, Stmt, Type, TypePath,
    parse::{Parse, ParseStream},
    parse_macro_input, parse_quote,
};

pub(crate) struct BlockingResultFn(pub(crate) ItemFn);

impl ToTokens for BlockingResultFn {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self(inner) = self;
        inner.to_tokens(tokens);
    }
}

impl Parse for BlockingResultFn {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut func: ItemFn = input.parse()?;
        let ItemFn {
            sig: Signature { output, .. },
            block,
            ..
        } = &mut func;

        ensure_output(output).ok_or_else(|| {
            input.error("this attribute should only annotate functions returning `anyhow::Result`")
        })?;

        handle_block(block);

        Ok(Self(func))
    }
}

pub(crate) fn handle_block(block: &mut Block) {
    block
        .stmts
        .iter_mut()
        .flat_map(|stmt| match stmt {
            Stmt::Local(Local {
                init: Some(LocalInit { expr, diverge, .. }),
                ..
            }) => {
                let base_iter = iter::once(Some(expr.as_mut()));
                if let Some((_, diverge)) = diverge {
                    Box::new(base_iter.chain(iter::once(Some(diverge.as_mut()))))
                        as Box<dyn Iterator<Item = Option<&mut Expr>>>
                } else {
                    Box::new(base_iter) as Box<dyn Iterator<Item = Option<&mut Expr>>>
                }
            }
            Stmt::Expr(expr, ..) => {
                Box::new(iter::once(Some(expr))) as Box<dyn Iterator<Item = Option<&mut Expr>>>
            }
            _ => Box::new(iter::once(None)) as Box<dyn Iterator<Item = Option<&mut Expr>>>,
        })
        .flatten()
        .for_each(handle_expr);
}

#[expect(
    clippy::too_many_lines,
    reason = "The limit hasn't been greatly surpassed, and the parser just needs it."
)]
pub(crate) fn handle_expr(expr: &mut Expr) {
    // NOTE: the recurrence relation is defined in terms of the base case for try
    // expressions (excluding try blocks,) and the case that triggers further
    // recursive calls (i.e. all other exprresions that may be `?`-annotated.)
    match expr {
        // The special case that is handled by this proc-macro.
        Expr::Try(ExprTry { expr, .. }) => {
            // TODO: test that this produces a block expression (in theory, it should,
            // considering the parser ought see the braces and the statements inside of it
            // as being part of the `stmts` field of a `Block` record.)
            *expr.as_mut() = parse_quote! {
                {
                    let res = #expr;
                    if res.is_err() && ::crossterm::terminal::is_raw_mode_enabled() {
                        ::tokio::task::spawn_blocking(|| ::crossterm::terminal::disable_raw_mode())
                            .await
                            .unwrap();
                    }
                    res
                }
            };
        }

        // All other possibly fallible (and thus `?`-annotated) expression types.
        Expr::Array(ExprArray { elems, .. }) | Expr::Tuple(ExprTuple { elems, .. }) => {
            elems.iter_mut().for_each(handle_expr);
        }
        Expr::Assign(ExprAssign { left, right, .. })
        | Expr::Binary(ExprBinary { left, right, .. }) => {
            handle_expr(left);
            handle_expr(right);
        }
        Expr::Block(ExprBlock { block, .. })
        | Expr::Const(PatConst { block, .. })
        | Expr::Loop(ExprLoop { body: block, .. })
        | Expr::Unsafe(ExprUnsafe { block, .. }) => {
            handle_block(block);
        }
        Expr::Break(ExprBreak {
            expr: Some(expr), ..
        })
        | Expr::Cast(ExprCast { expr, .. })
        | Expr::Group(ExprGroup { expr, .. })
        | Expr::Let(ExprLet { expr, .. })
        | Expr::Paren(ExprParen { expr, .. })
        | Expr::RawAddr(ExprRawAddr { expr, .. })
        | Expr::Reference(ExprReference { expr, .. })
        | Expr::Return(ExprReturn {
            expr: Some(expr), ..
        })
        | Expr::Unary(ExprUnary { expr, .. })
        | Expr::Field(ExprField { base: expr, .. })
        | Expr::Struct(ExprStruct {
            rest: Some(expr), ..
        }) => handle_expr(expr),
        Expr::Call(ExprCall {
            func: first_expr,
            args,
            ..
        })
        | Expr::MethodCall(ExprMethodCall {
            receiver: first_expr,
            args,
            ..
        }) => {
            handle_expr(first_expr);
            args.iter_mut().for_each(handle_expr);
        }
        Expr::ForLoop(ExprForLoop {
            expr, body: block, ..
        })
        | Expr::While(ExprWhile {
            cond: expr,
            body: block,
            ..
        }) => {
            handle_expr(expr);
            handle_block(block);
        }
        Expr::If(ExprIf {
            cond,
            then_branch,
            else_branch,
            ..
        }) => {
            handle_expr(cond);
            handle_block(then_branch);
            if let Some((_, else_branch)) = else_branch {
                handle_expr(else_branch);
            }
        }
        Expr::Index(ExprIndex {
            expr: first,
            index: second,
            ..
        })
        | Expr::Repeat(ExprRepeat {
            expr: first,
            len: second,
            ..
        }) => {
            handle_expr(first);
            handle_expr(second);
        }
        Expr::Match(ExprMatch { expr, arms, .. }) => {
            handle_expr(expr);
            for Arm { guard, body, .. } in arms.iter_mut() {
                if let Some((_, guard)) = guard {
                    handle_expr(guard);
                }
                handle_expr(body);
            }
        }
        Expr::Range(PatRange { start, end, .. }) => {
            if let Some(start) = start {
                handle_expr(start);
            }
            if let Some(end) = end {
                handle_expr(end);
            }
        }

        // Ignored cases.
        _ => (),
    }
}

pub(crate) fn ensure_output(output: &ReturnType) -> bool {
    match output {
        ReturnType::Type(_, ty)
            if let Type::Path(TypePath {
                path: Path { segments, .. },
                ..
            }) = &**ty
                && segments
                    .last()
                    .is_some_and(|PathSegment { ident, .. }| ident == "Result") =>
        {
            true
        }
        _ => false,
    }
}

#[proc_macro_attribute]
pub fn add(_: TokenStream, func: TokenStream) -> TokenStream {
    TokenStream::from(parse_macro_input!(func as BlockingResultFn).to_token_stream())
}
