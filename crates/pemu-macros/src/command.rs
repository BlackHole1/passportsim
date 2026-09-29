//! `#[command]`: turns a handler function into a registered `CommandSpec`. The grammar and an
//! example are documented on `pemu_api::registry`, which re-exports it.
//!
//! Everything checkable is checked during expansion: the name shape, the caps group, a one-line
//! summary from the first doc line, at least one example, consistent annotations, alias and
//! positional shapes, and the handler signature. An error code's name shape and its number lying in
//! the Core range or the command's own group range need the constants, so they expand into `const`
//! assertions.
//!
//! Natively the spec is registered in the `linkme` slice `pemu_api::registry::COMMANDS`; on wasm32
//! nothing names `linkme`, because `pemu-wasm`'s build script generates the list.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Error, Ident, ItemFn, LitStr, Path, Token, Type, bracketed, parenthesized, parse2};

/// On error the original function is kept, so the module still reports its own errors instead of a
/// cascade of "not found".
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    match try_expand(attr, item.clone()) {
        Ok(expanded) => expanded,
        Err(error) => {
            let mut out = item;
            out.extend(error.to_compile_error());
            out
        }
    }
}

fn try_expand(attr: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let func: ItemFn = parse2(item)?;
    let args: CommandArgs = parse2(attr)?;
    let spec = Spec::build(args, &func)?;
    Ok(spec.emit(&func))
}

enum SchemaSource {
    /// `input = RunArgs`: a `schemars::JsonSchema` type.
    Type(Type),
    /// `input_schema = run_args_schema`: an existing `fn() -> Schema`.
    Function(Path),
}

struct ExampleArg {
    title: LitStr,
    args: LitStr,
}

/// Before cross-checking, which `Spec::build` does.
struct CommandArgs {
    span: Span,
    name: Option<LitStr>,
    group: Option<Ident>,
    input: Option<SchemaSource>,
    output: Option<SchemaSource>,
    annotations: Vec<Ident>,
    positional: Vec<LitStr>,
    aliases: Vec<LitStr>,
    cli_only: Vec<LitStr>,
    scenario_step: Option<LitStr>,
    errors: Vec<Path>,
    examples: Vec<ExampleArg>,
    api_crate: Option<TokenStream>,
}

const KEYS: &str = "name, group, input, input_schema, output, output_schema, annotations, cli, \
                    scenario_step, errors, example, api_crate";

impl CommandArgs {
    fn empty(span: Span) -> Self {
        CommandArgs {
            span,
            name: None,
            group: None,
            input: None,
            output: None,
            annotations: Vec::new(),
            positional: Vec::new(),
            aliases: Vec::new(),
            cli_only: Vec::new(),
            scenario_step: None,
            errors: Vec::new(),
            examples: Vec::new(),
            api_crate: None,
        }
    }
}

fn set_once<T>(slot: &mut Option<T>, key: &Ident, value: T) -> syn::Result<()> {
    if slot.is_some() {
        return Err(Error::new(
            key.span(),
            format!("`{key}` is given twice in `#[command]`"),
        ));
    }
    *slot = Some(value);
    Ok(())
}

impl Parse for CommandArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = CommandArgs::empty(input.span());
        while !input.is_empty() {
            let key: Ident = input.parse().map_err(|e| {
                Error::new(
                    e.span(),
                    format!("expected a `#[command]` argument: {KEYS}"),
                )
            })?;
            match key.to_string().as_str() {
                "name" => {
                    input.parse::<Token![=]>()?;
                    set_once(&mut args.name, &key, input.parse()?)?;
                }
                "group" => {
                    input.parse::<Token![=]>()?;
                    set_once(&mut args.group, &key, input.parse()?)?;
                }
                "input" | "output" => {
                    input.parse::<Token![=]>()?;
                    let source = SchemaSource::Type(input.parse()?);
                    let slot = if key == "input" {
                        &mut args.input
                    } else {
                        &mut args.output
                    };
                    set_once(slot, &key, source)?;
                }
                "input_schema" | "output_schema" => {
                    input.parse::<Token![=]>()?;
                    let source = SchemaSource::Function(input.parse()?);
                    let slot = if key == "input_schema" {
                        &mut args.input
                    } else {
                        &mut args.output
                    };
                    set_once(slot, &key, source)?;
                }
                "scenario_step" => {
                    input.parse::<Token![=]>()?;
                    set_once(&mut args.scenario_step, &key, input.parse()?)?;
                }
                "api_crate" => {
                    input.parse::<Token![=]>()?;
                    let path = if input.peek(Token![crate]) {
                        input.parse::<Token![crate]>()?;
                        quote!(crate)
                    } else {
                        let path: Path = input.parse()?;
                        quote!(#path)
                    };
                    set_once(&mut args.api_crate, &key, path)?;
                }
                "annotations" => {
                    let content;
                    parenthesized!(content in input);
                    let flags = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?;
                    args.annotations.extend(flags);
                }
                "errors" => {
                    let content;
                    parenthesized!(content in input);
                    let codes = Punctuated::<Path, Token![,]>::parse_terminated(&content)?;
                    args.errors.extend(codes);
                }
                "cli" => {
                    let content;
                    parenthesized!(content in input);
                    parse_cli(&content, &mut args)?;
                }
                "example" => {
                    let content;
                    parenthesized!(content in input);
                    args.examples.push(parse_example(&content)?);
                }
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!("unknown `#[command]` argument `{other}`; expected one of {KEYS}"),
                    ));
                }
            }
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(args)
    }
}

/// `cli(positional = ["a", "b"], aliases = ["x"])`.
fn parse_cli(input: ParseStream, args: &mut CommandArgs) -> syn::Result<()> {
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let content;
        bracketed!(content in input);
        let values = Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?;
        match key.to_string().as_str() {
            "positional" => args.positional.extend(values),
            "aliases" => args.aliases.extend(values),
            // An input only a person may type. The MCP and HTTP schemas drop it, and the handler
            // still refuses it.
            "cli_only" => args.cli_only.extend(values),
            other => {
                return Err(Error::new(
                    key.span(),
                    format!(
                        "unknown `cli` argument `{other}`; expected `positional`, `aliases` or \
                         `cli_only`"
                    ),
                ));
            }
        }
        if input.is_empty() {
            break;
        }
        input.parse::<Token![,]>()?;
    }
    Ok(())
}

/// `example(title = "..", args = "{..}")`.
fn parse_example(input: ParseStream) -> syn::Result<ExampleArg> {
    let span = input.span();
    let mut title = None;
    let mut args = None;
    while !input.is_empty() {
        let key: Ident = input.parse()?;
        input.parse::<Token![=]>()?;
        let value: LitStr = input.parse()?;
        match key.to_string().as_str() {
            "title" => set_once(&mut title, &key, value)?,
            "args" => set_once(&mut args, &key, value)?,
            other => {
                return Err(Error::new(
                    key.span(),
                    format!("unknown `example` argument `{other}`; expected `title` or `args`"),
                ));
            }
        }
        if input.is_empty() {
            break;
        }
        input.parse::<Token![,]>()?;
    }
    match (title, args) {
        (Some(title), Some(args)) => Ok(ExampleArg { title, args }),
        (None, _) => Err(Error::new(span, "`example` needs a `title`")),
        (_, None) => Err(Error::new(span, "`example` needs `args`")),
    }
}

/// In the order `Annotations` declares them.
const ANNOTATIONS: [&str; 7] = [
    "read_only",
    "destructive",
    "idempotent",
    "advances_time",
    "needs_instance",
    "native_only",
    "human_confirm",
];

/// As attribute word and `CapsGroup` variant.
const GROUPS: [(&str, &str); 7] = [
    ("core", "Core"),
    ("audio", "Audio"),
    ("radio", "Radio"),
    ("nfc", "Nfc"),
    ("debug", "Debug"),
    ("device", "Device"),
    ("power", "Power"),
];

/// `[a-z][a-z0-9_]*`. Must agree with `pemu_api::spec::name_is_valid`.
fn name_is_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Spec {
    name: LitStr,
    group: Ident,
    summary: String,
    input: Option<SchemaSource>,
    output: Option<SchemaSource>,
    annotations: Vec<Ident>,
    positional: Vec<LitStr>,
    aliases: Vec<LitStr>,
    cli_only: Vec<LitStr>,
    scenario_step: Option<LitStr>,
    errors: Vec<Path>,
    examples: Vec<ExampleArg>,
    api: TokenStream,
}

impl Spec {
    fn build(args: CommandArgs, func: &ItemFn) -> syn::Result<Self> {
        let span = args.span;
        check_handler_signature(func)?;

        let name = args
            .name
            .ok_or_else(|| Error::new(span, "`#[command]` needs `name = \"...\"`"))?;
        if !name_is_valid(&name.value()) {
            return Err(Error::new(
                name.span(),
                "a command name is lowercase ASCII letters, digits and `_`, starting with a \
                 letter",
            ));
        }

        let group = args.group.ok_or_else(|| {
            Error::new(
                span,
                "`#[command]` needs `group = core|audio|radio|nfc|debug|device|power`",
            )
        })?;
        let group_variant = GROUPS
            .iter()
            .find(|(word, _)| group == word)
            .map(|(_, variant)| Ident::new(variant, group.span()))
            .ok_or_else(|| {
                Error::new(
                    group.span(),
                    "unknown caps group; expected core, audio, radio, nfc, debug, device or power",
                )
            })?;

        let summary = summary_of(func)?;
        check_annotations(&args.annotations, &group)?;
        check_cli(&args.positional, &args.aliases, &name)?;
        check_cli_only(&args.cli_only)?;
        check_scenario_step(args.scenario_step.as_ref())?;
        check_errors(&args.errors)?;
        check_examples(&args.examples, span)?;

        Ok(Spec {
            name,
            group: group_variant,
            summary,
            input: args.input,
            output: args.output,
            annotations: args.annotations,
            positional: args.positional,
            aliases: args.aliases,
            cli_only: args.cli_only,
            scenario_step: args.scenario_step,
            errors: args.errors,
            examples: args.examples,
            api: args.api_crate.unwrap_or_else(|| quote!(::pemu_api)),
        })
    }
}

/// So it can become a `CommandSpec::handler` function pointer.
fn check_handler_signature(func: &ItemFn) -> syn::Result<()> {
    let sig = &func.sig;
    if let Some(token) = sig.asyncness {
        return Err(Error::new(
            token.span,
            "a command handler is not `async`: virtual time advances inside the call",
        ));
    }
    if let Some(token) = sig.constness {
        return Err(Error::new(token.span, "a command handler is not `const`"));
    }
    if let syn::Safety::Unsafe(token) = sig.safety {
        return Err(Error::new(token.span, "a command handler is not `unsafe`"));
    }
    if !sig.generics.params.is_empty() {
        return Err(Error::new(
            sig.generics.span(),
            "a command handler takes no generic parameters: `CommandSpec::handler` is a function \
             pointer",
        ));
    }
    if sig.inputs.len() != 2 {
        return Err(Error::new(
            sig.inputs.span(),
            "a command handler takes exactly two arguments, `&mut HandlerCx` and the JSON \
             arguments",
        ));
    }
    if matches!(sig.inputs.first(), Some(syn::FnArg::Receiver(_))) {
        return Err(Error::new(
            sig.inputs.span(),
            "a command handler is a free function, not a method",
        ));
    }
    if matches!(sig.output, syn::ReturnType::Default) {
        return Err(Error::new(
            sig.ident.span(),
            "a command handler returns `Result<Output, ApiError>`",
        ));
    }
    Ok(())
}

/// The first non-empty doc line, reused verbatim by CLI help, the MCP tool description and the
/// generated docs.
fn summary_of(func: &ItemFn) -> syn::Result<String> {
    for attr in &func.attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) = &nv.value
        else {
            continue;
        };
        let line = text.value().trim().to_string();
        if !line.is_empty() {
            return Ok(line);
        }
    }
    Err(Error::new(
        func.sig.ident.span(),
        "a command needs a one-line doc comment: it is the summary reused by CLI help, the MCP \
         tool description and the generated docs",
    ))
}

/// The same rules as `Annotations::check`, checked here so the message points at the word.
fn check_annotations(flags: &[Ident], group: &Ident) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for flag in flags {
        let word = flag.to_string();
        if !ANNOTATIONS.contains(&word.as_str()) {
            return Err(Error::new(
                flag.span(),
                format!(
                    "unknown annotation `{word}`; expected one of {}",
                    ANNOTATIONS.join(", ")
                ),
            ));
        }
        if seen.contains(&word) {
            return Err(Error::new(flag.span(), format!("`{word}` is listed twice")));
        }
        seen.push(word);
    }
    let has = |word: &str| seen.iter().any(|s| s == word);
    if has("read_only") && has("destructive") {
        return Err(Error::new(
            flags[0].span(),
            "`read_only` and `destructive` exclude each other",
        ));
    }
    if has("destructive") && !has("human_confirm") {
        return Err(Error::new(
            flags[0].span(),
            "`destructive` needs `human_confirm`: every destructive step needs a human \
             confirmation",
        ));
    }
    if group == "device" && !has("native_only") {
        return Err(Error::new(
            group.span(),
            "the `device` group is `native_only`: it is absent from the wasm build",
        ));
    }
    Ok(())
}

fn check_cli_only(cli_only: &[LitStr]) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for value in cli_only {
        let text = value.value();
        if !name_is_valid(&text) {
            return Err(Error::new(
                value.span(),
                "a `cli_only` value names an input property: lowercase ASCII letters, digits and \
                 `_`",
            ));
        }
        if seen.contains(&text) {
            return Err(Error::new(
                value.span(),
                "this `cli_only` value is listed twice",
            ));
        }
        seen.push(text);
    }
    Ok(())
}

/// They become clap subcommand names and positional arguments.
fn check_cli(positional: &[LitStr], aliases: &[LitStr], name: &LitStr) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for value in positional {
        let text = value.value();
        if !name_is_valid(&text) {
            return Err(Error::new(
                value.span(),
                "a positional names an input property: lowercase ASCII letters, digits and `_`",
            ));
        }
        if seen.contains(&text) {
            return Err(Error::new(value.span(), "this positional is listed twice"));
        }
        seen.push(text);
    }
    let mut seen: Vec<String> = vec![name.value()];
    for value in aliases {
        let text = value.value();
        if !name_is_valid(&text) {
            return Err(Error::new(
                value.span(),
                "an alias is a registry name: lowercase ASCII letters, digits and `_`, starting \
                 with a letter",
            ));
        }
        if seen.contains(&text) {
            return Err(Error::new(
                value.span(),
                "this alias repeats the command name or another alias",
            ));
        }
        seen.push(text);
    }
    Ok(())
}

/// Optionally dotted, as in `serial.write`.
fn check_scenario_step(step: Option<&LitStr>) -> syn::Result<()> {
    let Some(step) = step else {
        return Ok(());
    };
    let text = step.value();
    if text.is_empty() || !text.split('.').all(name_is_valid) {
        return Err(Error::new(
            step.span(),
            "a scenario step key is a registry name, optionally dotted, as in `serial.write`",
        ));
    }
    Ok(())
}

/// Name shape and group range are checked by the emitted `const` assertions, which need the value.
fn check_errors(errors: &[Path]) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for code in errors {
        let text = quote!(#code).to_string();
        if seen.contains(&text) {
            return Err(Error::new(code.span(), "this error code is listed twice"));
        }
        seen.push(text);
    }
    Ok(())
}

/// Only the shape is checked here; `pemu_api::registry::check` parses the JSON at run time.
fn check_examples(examples: &[ExampleArg], span: Span) -> syn::Result<()> {
    if examples.is_empty() {
        return Err(Error::new(
            span,
            "a command needs at least one `example(title = \"..\", args = \"{..}\")`: CI runs it \
             against a fixture instance",
        ));
    }
    let mut titles: Vec<String> = Vec::new();
    for example in examples {
        let title = example.title.value();
        if title.trim().is_empty() {
            return Err(Error::new(
                example.title.span(),
                "an example title says in one line what the example shows",
            ));
        }
        if titles.contains(&title) {
            return Err(Error::new(
                example.title.span(),
                "two examples share this title",
            ));
        }
        titles.push(title);
        let args = example.args.value();
        let args = args.trim();
        if !(args.starts_with('{') && args.ends_with('}')) {
            return Err(Error::new(
                example.args.span(),
                "example `args` is a JSON object, the same shape a command call takes",
            ));
        }
    }
    Ok(())
}

impl Spec {
    fn emit(&self, func: &ItemFn) -> TokenStream {
        let api = &self.api;
        let handler = &func.sig.ident;
        let upper = handler.to_string().to_uppercase();
        let spec_ident = format_ident!("SPEC_{}", upper, span = handler.span());
        let register_ident = format_ident!("REGISTER_{}", upper, span = handler.span());
        let name = &self.name;
        let group = &self.group;
        let summary = &self.summary;
        let (input_item, input_schema) = self.schema_fn(self.input.as_ref(), handler, "input", api);
        let (output_item, output_schema) =
            self.schema_fn(self.output.as_ref(), handler, "output", api);
        let flags = self.annotations.iter();
        let positional = self.positional.iter();
        let aliases = self.aliases.iter();
        let cli_only = self.cli_only.iter();
        let scenario_step = match &self.scenario_step {
            Some(step) => quote!(Some(#step)),
            None => quote!(None),
        };
        let examples = self.examples.iter().map(|example| {
            let title = &example.title;
            let args = &example.args;
            quote!(#api::spec::Example { title: #title, args: #args })
        });
        let errors = self.errors.iter();
        let asserts = self.errors.iter().map(|code| {
            quote! {
                assert!(
                    #code.name_is_valid(),
                    "an error code is named `E_` plus `A-Z`, `0-9` and `_`"
                );
                assert!(
                    #code.allowed_in(#api::spec::CapsGroup::#group),
                    "an error code number lies in the Core range or in the command's own caps \
                     group range"
                );
            }
        });
        let doc = format!(
            "`CommandSpec` of the `{}` command, registered by `#[command]`. \
             The generated wasm32 command list names this constant.",
            name.value()
        );

        quote! {
            #func

            #input_item
            #output_item

            #[doc = #doc]
            pub const #spec_ident: #api::spec::CommandSpec = #api::spec::CommandSpec {
                name: #name,
                group: #api::spec::CapsGroup::#group,
                summary: #summary,
                input_schema: #input_schema,
                output_schema: #output_schema,
                annotations: #api::spec::Annotations {
                    #(#flags: true,)*
                    ..#api::spec::Annotations::EMPTY
                },
                cli: #api::spec::CliShape {
                    positional: &[#(#positional),*],
                    aliases: &[#(#aliases),*],
                    cli_only: &[#(#cli_only),*],
                },
                scenario_step: #scenario_step,
                examples: &[#(#examples),*],
                errors: &[#(#errors),*],
                handler: #handler,
            };

            #[cfg(not(target_arch = "wasm32"))]
            #[doc(hidden)]
            #[#api::registry::linkme::distributed_slice(#api::registry::COMMANDS)]
            #[linkme(crate = #api::registry::linkme)]
            static #register_ident: #api::spec::CommandSpec = #spec_ident;

            const _: () = {
                #(#asserts)*
            };
        }
    }

    /// Plus the item that defines it when the command named a type. Without either, every JSON
    /// value is accepted (`any_schema`).
    fn schema_fn(
        &self,
        source: Option<&SchemaSource>,
        handler: &Ident,
        side: &str,
        api: &TokenStream,
    ) -> (TokenStream, TokenStream) {
        match source {
            None => (quote!(), quote!(#api::spec::any_schema)),
            Some(SchemaSource::Function(path)) => (quote!(), quote!(#path)),
            Some(SchemaSource::Type(ty)) => {
                let ident = format_ident!("__{}_{}_schema", handler, side, span = handler.span());
                let doc = format!("JSON Schema of the {side} of `{handler}`.");
                let item = quote! {
                    #[doc = #doc]
                    #[doc(hidden)]
                    pub fn #ident() -> #api::spec::Schema {
                        #api::spec::schemars::SchemaGenerator::default()
                            .into_root_schema_for::<#ty>()
                    }
                };
                (item, quote!(#ident))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDLER: &str = "/// Advance virtual time until a matcher fires.\n\
                           fn run(cx: &mut HandlerCx, args: serde_json::Value) \
                           -> Result<Output, ApiError> { todo!() }";

    fn build(attr: &str, item: &str) -> syn::Result<Spec> {
        let func: ItemFn = syn::parse_str(item)?;
        let args: CommandArgs = parse2(attr.parse().expect("the attribute tokenizes"))?;
        Spec::build(args, &func)
    }

    /// `Spec` has no `Debug`, so `expect_err` is out.
    fn message(built: syn::Result<Spec>) -> String {
        match built {
            Ok(spec) => panic!("`{}` was accepted but should not be", spec.name.value()),
            Err(error) => error.to_string(),
        }
    }

    fn error(attr: &str) -> String {
        message(build(attr, HANDLER))
    }

    fn valid() -> String {
        String::from(
            "name = \"run\", group = core, \
             annotations(advances_time, needs_instance), \
             cli(positional = [\"until\"], aliases = [\"r\"]), \
             scenario_step = \"wait\", errors(E_TIMEOUT), \
             example(title = \"Wait for the menu\", args = r#\"{\"timeout\":\"5s\"}\"#)",
        )
    }

    #[test]
    fn a_complete_attribute_parses() {
        let spec = build(&valid(), HANDLER).expect("the attribute is valid");
        assert_eq!(spec.name.value(), "run");
        assert_eq!(spec.group, "Core");
        assert_eq!(spec.summary, "Advance virtual time until a matcher fires.");
        assert_eq!(spec.annotations.len(), 2);
        assert_eq!(spec.positional[0].value(), "until");
        assert_eq!(spec.aliases[0].value(), "r");
        assert_eq!(spec.scenario_step.expect("a step").value(), "wait");
        assert_eq!(spec.errors.len(), 1);
        assert_eq!(spec.examples.len(), 1);
        assert!(spec.api.to_string().contains("pemu_api"));
    }

    #[test]
    fn an_expansion_registers_natively_and_names_no_linkme_item_for_wasm() {
        let func: ItemFn = syn::parse_str(HANDLER).expect("the handler parses");
        let spec = build(&valid(), HANDLER).expect("the attribute is valid");
        let text = spec.emit(&func).to_string();
        assert!(text.contains("SPEC_RUN"));
        assert!(text.contains("distributed_slice"));
        // Every linkme item sits behind the same `cfg`.
        let gate = "# [cfg (not (target_arch = \"wasm32\"))]";
        let before = text.split("distributed_slice").next().expect("a prefix");
        assert!(
            before.contains(gate),
            "the linkme element is not gated: {text}"
        );
        assert_eq!(text.matches("linkme").count(), 3);
        assert_eq!(text.matches(gate).count(), 1);
    }

    #[test]
    fn the_group_picks_the_caps_group_variant() {
        for (word, variant) in GROUPS {
            let attr = valid().replace("group = core", &format!("group = {word}"));
            let attr = if word == "device" {
                attr.replace("annotations(", "annotations(native_only, ")
            } else {
                attr
            };
            let spec = build(&attr, HANDLER).expect("the group is valid");
            assert_eq!(spec.group, variant);
        }
    }

    #[test]
    fn a_missing_name_group_summary_or_example_is_rejected() {
        assert!(error("group = core").contains("needs `name"));
        assert!(error("name = \"run\"").contains("needs `group"));
        let without_example = valid().replace(
            "example(title = \"Wait for the menu\", args = r#\"{\"timeout\":\"5s\"}\"#)",
            "",
        );
        assert!(error(without_example.trim_end_matches([' ', ','])).contains("at least one"));
        let undocumented = "fn run(cx: &mut HandlerCx, args: serde_json::Value) \
                            -> Result<Output, ApiError> { todo!() }";
        let text = message(build(&valid(), undocumented));
        assert!(text.contains("one-line doc comment"), "{text}");
    }

    #[test]
    fn a_bad_name_group_alias_or_step_is_rejected() {
        let with = |old: &str, new: &str| valid().replace(old, new);
        assert!(error(&with("\"run\"", "\"Run\"")).contains("lowercase ASCII"));
        assert!(error(&with("\"run\"", "\"1run\"")).contains("lowercase ASCII"));
        assert!(error(&with("group = core", "group = nfd")).contains("unknown caps group"));
        assert!(error(&with("[\"r\"]", "[\"run\"]")).contains("repeats the command name"));
        assert!(error(&with("[\"r\"]", "[\"R\"]")).contains("an alias is a registry name"));
        assert!(error(&with("\"wait\"", "\"wait.\"")).contains("scenario step key"));
    }

    #[test]
    fn contradicting_annotations_are_rejected() {
        let with =
            |flags: &str| valid().replace("annotations(advances_time, needs_instance)", flags);
        assert!(
            error(&with("annotations(read_only, destructive, human_confirm)"))
                .contains("exclude each other")
        );
        assert!(error(&with("annotations(destructive)")).contains("needs `human_confirm`"));
        assert!(error(&with("annotations(read_only, read_only)")).contains("listed twice"));
        assert!(error(&with("annotations(readonly)")).contains("unknown annotation"));
        let device = valid().replace("group = core", "group = device");
        assert!(error(&device).contains("`native_only`"));
    }

    #[test]
    fn a_repeated_key_an_unknown_key_and_a_bad_example_are_rejected() {
        assert!(error(&format!("{}, name = \"other\"", valid())).contains("given twice"));
        assert!(error(&format!("{}, colour = \"red\"", valid())).contains("unknown `#[command]`"));
        assert!(error(&valid().replace("cli(positional", "cli(order")).contains("unknown `cli`"));
        let not_an_object = "name = \"run\", group = core, \
                             example(title = \"An array\", args = \"[1]\")";
        assert!(error(not_an_object).contains("JSON object"));
        assert!(
            error(&valid().replace("errors(E_TIMEOUT)", "errors(E_TIMEOUT, E_TIMEOUT)"))
                .contains("listed twice")
        );
        assert!(
            error(&valid().replace("title = \"Wait for the menu\"", "title = \"\""))
                .contains("in one line")
        );
    }

    #[test]
    fn a_handler_that_is_not_a_plain_two_argument_fn_is_rejected() {
        let cases = [
            (
                "/// Doc.\nasync fn run(cx: &mut HandlerCx, args: serde_json::Value) \
                 -> Result<Output, ApiError> { todo!() }",
                "`async`",
            ),
            (
                "/// Doc.\nfn run(cx: &mut HandlerCx) -> Result<Output, ApiError> { todo!() }",
                "exactly two arguments",
            ),
            (
                "/// Doc.\nfn run<T>(cx: &mut HandlerCx, args: T) -> Result<Output, ApiError> \
                 { todo!() }",
                "no generic parameters",
            ),
            (
                "/// Doc.\nfn run(cx: &mut HandlerCx, args: serde_json::Value) { todo!() }",
                "returns `Result<Output, ApiError>`",
            ),
        ];
        for (item, expected) in cases {
            let text = message(build(&valid(), item));
            assert!(text.contains(expected), "{text}");
        }
    }

    #[test]
    fn the_api_crate_override_replaces_the_default_path() {
        let spec = build(&format!("api_crate = crate, {}", valid()), HANDLER)
            .expect("the attribute is valid");
        assert_eq!(spec.api.to_string(), "crate");
        let spec = build(&format!("api_crate = ::other_api, {}", valid()), HANDLER)
            .expect("the attribute is valid");
        assert_eq!(spec.api.to_string(), ":: other_api");
    }

    #[test]
    fn a_named_type_becomes_a_generated_schema_function() {
        let func: ItemFn = syn::parse_str(HANDLER).expect("the handler parses");
        let attr = format!(
            "{}, input = RunArgs, output_schema = run_output_schema",
            valid()
        );
        let text = build(&attr, HANDLER)
            .expect("the attribute is valid")
            .emit(&func)
            .to_string();
        assert!(text.contains("__run_input_schema"));
        assert!(text.contains("into_root_schema_for :: < RunArgs >"));
        assert!(text.contains("output_schema : run_output_schema"));
    }

    #[test]
    fn the_macro_and_the_crate_agree_on_the_name_shape() {
        for name in ["run", "net_http", "a", "b2"] {
            assert!(name_is_valid(name), "{name}");
        }
        for name in ["", "Run", "1run", "run-fast", "run.fast", "run "] {
            assert!(!name_is_valid(name), "{name}");
        }
    }
}
