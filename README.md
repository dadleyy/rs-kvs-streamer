# `rs-kvs-streamer`

This is a pure-rust implementation/demonstration of how one might build an application
that streams video data to amazon web service's [kinesis video streams (kvs)][kvs].

The majority of the implementation here is combining the apis of several multimedia
crates, which are truly performing the heavy lifting:

1. [opencv] - this crate is used to interact with the camera hardware.
2. [openh264] - once we have video data, kvs prefers it to be encoded in the [h264] format.
2. [webm_iterable] - once we have encoded data, we need to put it in the [MKV] media container.

### Running

Currently, this crate exposes a single binary (`src/main.rs`), which uses [clap] to provide
a command line interface.

Credentials are loaded from the environment, and [dotenv] is being used to source an optional
`.env` file from the current working directory. A typical `.env` file would look like:

```
AWS_STREAM_ACCESS_KEY=...
AWS_STREAM_SECRET_KEY=...
AWS_STREAM_NAME=...
AWS_STREAM_ARN=...
AWS_STREAM_REGION=...
```

With a `.env` file prepared, the command line help can be viewed by running:

```
cargo run -- --help
```


[kvs]: https://aws.amazon.com/kinesis/video-streams/
[h264]: https://en.wikipedia.org/wiki/Advanced_Video_Coding
[opencv]: https://crates.io/crates/opencv
[openh264]: https://crates.io/crates/openh264
[webm_iterable]: https://crates.io/crates/webm-iterable
[clap]: https://crates.io/crates/clap
[dotenv]: https://crates.io/crates/dotenv
[MKV]: https://www.matroska.org/technical/notes.html
