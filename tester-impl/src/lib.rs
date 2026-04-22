#![feature(bool_to_result)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use syn::{
    ItemFn, Path, PathSegment, ReturnType, Signature, Type, TypePath,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

pub(crate) struct BlockingResultFn;

impl BlockingResultFn {
    fn tokenize(self) -> TokenStream2 {
        todo!()
    }
}

impl Parse for BlockingResultFn {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let func: ItemFn = input.parse()?;
        let ItemFn {
            sig: Signature { output, .. },
            block,
            ..
        } = &func;

        ensure_output(output).ok_or_else(|| {
            input.error("this attribute should only annotate functions returning `anyhow::Result`")
        })?;

        // TODO: parse the function body for fallible operations annotated with `?`.

        todo!()
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
    TokenStream::from(parse_macro_input!(func as BlockingResultFn).tokenize())
}
