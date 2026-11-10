# model-registry

A lightweight, Rust-powered CLI for managing, versioning ML models.

## Hot-reload sample

The `mr-simple-test` example loads `model.json` from a model version. The file should
contain a non-empty `message` field, for example:

```json
{"message":"hello from v1"}
```

Run it with AWS credentials configured and an existing model/tag:

```sh
cargo run --bin mr-simple-test -- <bucket> <model> <tag> [interval-secs] [cache-dir]
```

It prints the active version and message. To test a reload, push another version
containing an updated `model.json`, then move the tag while the sample is running:

```sh
cargo run --bin mr -- tag <model>:<new-version> <tag>
```

The sample should print the new version and message after the next polling interval.
