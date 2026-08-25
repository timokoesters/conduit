use proc_macro::TokenStream;
use quote::quote;

#[cfg(feature = "doc-generators")]
mod doc_generators;

/// Allows for the doc comments of enums to be accessed at runtime.
#[cfg(feature = "doc-generators")]
#[proc_macro_derive(DocumentEnum)]
pub fn document_restrictions(item: TokenStream) -> TokenStream {
    use doc_generators::enums::Enum;
    use syn::parse_macro_input;

    let docs = parse_macro_input!(item as Enum);

    quote! { #docs }.into()
}

/// Allows for the doc comments of structs to be accessed at runtime.
#[cfg(feature = "doc-generators")]
#[proc_macro_derive(DocumentStruct)]
pub fn document_struct(item: TokenStream) -> TokenStream {
    use doc_generators::structs::Struct;
    use syn::parse_macro_input;

    let docs = parse_macro_input!(item as Struct);

    quote! { #docs }.into()
}

#[cfg(feature = "doc-generators")]
#[proc_macro_derive(GetStructFieldValue)]
pub fn get_struct_field_value(item: TokenStream) -> TokenStream {
    use doc_generators::get_struct_field_values::Struct;
    use syn::parse_macro_input;

    let docs = parse_macro_input!(item as Struct);

    quote! { #docs }.into()
}

#[cfg(feature = "rate-limiting")]
#[proc_macro_attribute]
pub fn rate_limiting(attr: TokenStream, item: TokenStream) -> TokenStream {
    use syn::{Block, ItemFn, Path, parse_macro_input};

    let attr = parse_macro_input!(attr as Path);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = parse_macro_input!(item as ItemFn);

    let Block {
        brace_token: _,
        stmts,
    } = *block;

    let rate_limiting = quote! {
        services().rate_limiting.check(body.sender.clone().into(), conduit_config::rate_limiting::#attr.into()).await?;
    };

    quote! {
        #(#attrs)*
        #vis #sig {
            #rate_limiting

            #(#stmts)*
        }
    }
    .into()
}
