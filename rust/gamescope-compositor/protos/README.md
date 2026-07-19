# Gamescope Perfetto TrackEvent extension

Gamescope records event data with the typed `gamescope_event` TrackEvent
extension instead of debug annotations. Event and variant names are static
TrackEvents; runtime values are protobuf fields queryable as
`gamescope_event.<field_name>` in Trace Processor.

Trace Processor needs the extension descriptors to decode those fields. The
checked-in `gamescope_tracing_descriptors.gz` bundle can be served directly.
Regenerate and validate it with a Perfetto checkout next to Gamescope:

```sh
../perfetto/out/linux/tracing_proto_extensions \
  --json rust/gamescope-compositor/protos/extensions.json \
  -I . \
  -I rust/gamescope-compositor/protos \
  -I ../perfetto \
  --descriptor-out \
    rust/gamescope-compositor/protos/gamescope_tracing_descriptors.gz \
  --gzip
```

Make that descriptor set available through a Perfetto Extension Server, or
merge it into the tracing service's descriptor bundle. See Perfetto's
`docs/instrumentation/extensions.md` for descriptor delivery options.

The Rust field-number enum in `src/perfetto.rs` and the two proto files must be
updated together. Regenerating the bundle also validates `extensions.json` and
the allocated range.
