use std::iter;

use syn::{
    Arm, Block, Expr, ExprArray, ExprAssign, ExprBinary, ExprBlock, ExprBreak, ExprCall, ExprCast,
    ExprField, ExprForLoop, ExprGroup, ExprIf, ExprIndex, ExprLet, ExprLoop, ExprMatch,
    ExprMethodCall, ExprParen, ExprRawAddr, ExprReference, ExprRepeat, ExprReturn, ExprStruct,
    ExprTry, ExprTuple, ExprUnary, ExprUnsafe, ExprWhile, Local, LocalInit, PatConst, PatRange,
    Stmt, parse_quote,
};

pub(crate) fn handle_block(block: &mut Block) {
    macro_rules! cast {
        ($it:expr) => {{ Box::new($it) as Box<dyn Iterator<Item = Option<&mut Expr>>> }};
    }

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
                    cast!(base_iter.chain(iter::once(Some(diverge.as_mut()))))
                } else {
                    cast!(base_iter)
                }
            }
            Stmt::Expr(expr, ..) => cast!(iter::once(Some(expr))),
            _ => cast!(iter::once(None)),
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
            *expr.as_mut() = parse_quote! {{
                let res = #expr;

                if res.is_err() && ::crossterm::terminal::is_raw_mode_enabled().unwrap() {
                    ::tokio::task::spawn_blocking(::crossterm::terminal::disable_raw_mode)
                        .await
                        .unwrap()
                        .unwrap();
                }

                res
            }}
        }

        // All other possibly fallible (and thus `?`-annotated) expression types.
        Expr::Array(ExprArray { elems, .. }) | Expr::Tuple(ExprTuple { elems, .. }) => {
            elems.iter_mut().for_each(handle_expr);
        }
        Expr::Assign(ExprAssign {
            left: first,
            right: second,
            ..
        })
        | Expr::Binary(ExprBinary {
            left: first,
            right: second,
            ..
        })
        | Expr::Index(ExprIndex {
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
        Expr::Match(ExprMatch { expr, arms, .. }) => {
            handle_expr(expr);

            arms.iter_mut()
                .map(|Arm { guard, body, .. }| (guard, body))
                .for_each(|(guard, body)| {
                    if let Some((_, guard)) = guard {
                        handle_expr(guard);
                    }

                    handle_expr(body);
                });
        }
        Expr::Range(PatRange { start, end, .. }) => {
            if let Some(start) = start {
                handle_expr(start);
            }

            if let Some(end) = end {
                handle_expr(end);
            }
        }

        // Ignored cases with no fallible expressions within them.
        _ => (),
    }
}
