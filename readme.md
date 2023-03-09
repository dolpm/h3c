### example usage
```rust
let mut c = Client::connect("google.com").expect("failed to ocnnect to server");
let resp_bytes = c
    .send(
        url::Url::from_str(
            "https://www.google.com",
        )
        .unwrap(),
        "GET",
    )
    .unwrap();
println!("body: {}", std::str::from_utf8(resp_bytes).unwrap());
```