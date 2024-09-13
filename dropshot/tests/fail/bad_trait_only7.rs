// Copyright 2024 Oxide Computer Company

#![allow(unused_imports)]

// Test for a context type with a lifetime parameter.

#[dropshot::api_description]
trait MyApi {
    type Context<'a>;
}

enum MyImpl {}

// This should not produce errors about the trait or the context type being
// missing.
impl MyApi for MyImpl {
    type Context<'a> = ();
}

fn main() {
    // These items will NOT be present because of the invalid context type, and
    // will cause errors to be generated.
    my_api_mod::api_description::<MyImpl>();
    my_api_mod::stub_api_description();
}
