//! `#[test_id(t7k3m9q)] #[test] fn name() { … }` expands to
//! `mod name { use super::*; #[test] fn t7k3m9q() { … } }`, so the test is listed as
//! `…::name::t7k3m9q`: the name stays for people, the last segment is the id.
//! Ids are stamped by `test-registry stamp`, never typed.

use proc_macro::{Delimiter, Group, Ident, Span, TokenStream, TokenTree};

fn compile_error(msg: &str) -> TokenStream {
    format!("compile_error!({msg:?});")
        .parse()
        .expect("compile_error tokens")
}

fn expand(args: TokenStream, item: TokenStream) -> Result<TokenStream, String> {
    let mut a = args.into_iter();
    let id = match (a.next(), a.next()) {
        (Some(TokenTree::Ident(i)), None) => i.to_string(),
        _ => return Err("#[test_id] takes exactly one identifier".into()),
    };
    let tokens: Vec<TokenTree> = item.into_iter().collect();
    let kw = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Ident(i) if i.to_string() == "fn"))
        .ok_or("#[test_id] applies to a fn")?;
    let name = match tokens.get(kw + 1) {
        Some(TokenTree::Ident(n)) => n.to_string(),
        _ => return Err("#[test_id] applies to a fn".into()),
    };
    let renamed: TokenStream = tokens
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            if i == kw + 1 {
                TokenTree::Ident(Ident::new(&id, Span::call_site()))
            } else {
                t
            }
        })
        .collect();
    let mut body: TokenStream = "use super::*;".parse().expect("use tokens");
    body.extend(renamed);
    let mut out: TokenStream = format!("mod {name}")
        .parse()
        .map_err(|_| "bad fn name".to_string())?;
    out.extend([TokenTree::Group(Group::new(Delimiter::Brace, body))]);
    Ok(out)
}

#[proc_macro_attribute]
pub fn test_id(args: TokenStream, item: TokenStream) -> TokenStream {
    expand(args, item).unwrap_or_else(|e| compile_error(&e))
}
