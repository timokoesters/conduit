use syn::{Attribute, Expr, Lit, MetaNameValue};

pub mod enums;
pub mod get_struct_field_values;
pub mod structs;

fn attrs_to_doc_comment(attrs: Vec<Attribute>) -> String {
    attrs
        .into_iter()
        .filter_map(|attr| {
            if let syn::Meta::NameValue(MetaNameValue { path, value, .. }) = attr.meta
                && path.is_ident("doc")
                && let Expr::Lit(lit) = value
                && let Lit::Str(string) = lit.lit
            {
                Some(string.value().trim().to_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
