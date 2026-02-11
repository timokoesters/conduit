#[cfg(feature = "doc-generators")]
use proc_macro::TokenStream;
use syn::parse::{Parse, ParseStream};

#[cfg(feature = "doc-generators")]
mod doc_generators;

#[cfg(feature = "doc-generators")]
#[proc_macro_attribute]
pub fn document_restrictions(_attr: TokenStream, item: TokenStream) -> TokenStream {
    todo!()
}

fn parse_one_or_more<T: Parse>(input: ParseStream) -> syn::Result<Vec<T>> {
    let mut result = Vec::new();
    result.push(input.parse()?);

    while let Ok(item) = input.parse() {
        result.push(item);
    }

    Ok(result)
}
