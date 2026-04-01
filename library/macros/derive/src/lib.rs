mod abstraction;

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn abstraction(attr: TokenStream, item: TokenStream) -> TokenStream {
    abstraction::abstraction_impl(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
