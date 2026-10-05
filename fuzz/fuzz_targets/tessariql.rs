//! The language parser, on any text: a script, an expression and a read.
#![no_main]

libfuzzer_sys::fuzz_target!(|text: &str| {
    drop(tessari_ql::parse(text));
    drop(tessari_ql::parse_expression(text));
    drop(tessari_ql::parse_read(text));
});
