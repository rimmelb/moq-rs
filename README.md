<p align="center">
	<img height="128px" src="https://github.com/kixelated/moq-rs/blob/main/.github/logo.svg" alt="Media over QUIC">
</p>

Media over QUIC (MoQ) is a live media delivery protocol utilizing QUIC streams.
See [quic.video](https://quic.video) for more information.

This repository contains a few crates:

-   **moq-relay**: Accepting content from publishers and serves it to any subscribers.
-   **moq-pub**: Publishes fMP4 broadcasts.
-   **moq-transport**: An implementation of the underlying MoQ protocol.
-   **moq-api**: A HTTP API server that stores the origin for each broadcast, backed by redis.
-   **moq-dir**: Aggregates announcements, used to discover broadcasts.
-   **moq-clock**: A dumb clock client/server just to prove MoQ is more than media.

There's currently no way to view media with this repo; you'll need to use [moq-js](https://github.com/kixelated/moq-js) for that.
A hosted version is available at [quic.video](https://quic.video) and accepts the `?host=localhost:4443` query parameter.

# Development

Launch a basic cluster, including provisioning certs and deploying root certificates:

```
make run
```

Then, visit https://quic.video/publish/?server=localhost:4443.

For more control, use the [dev helper scripts](dev/README.md).

# Usage

## moq-relay

[moq-relay](moq-relay) is a server that forwards subscriptions from publishers to subscribers, caching and deduplicating along the way.
It's designed to be run in a datacenter, relaying media across multiple hops to deduplicate and improve QoS.
The relays optionally register themselves via the [moq-api](moq-api) endpoints, which is used to discover other relays and share broadcasts.

Notable arguments:

-   `--bind <ADDR>` Listen on this address, default: `[::]:4443`
-   `--tls-cert <CERT>` Use the certificate file at this path
-   `--tls-key <KEY>` Use the private key at this path
-   `--announce <URL>` Forward all announcements to this instance, typically [moq-dir](moq-dir).

This listens for WebTransport connections on `UDP https://localhost:4443` by default.
You need a client to connect to that address, to both publish and consume media.

## moq-pub

A client that publishes a fMP4 stream over MoQ, with a few restrictions.

-   `separate_moof`: Each fragment must contain a single track.
-   `frag_keyframe`: A keyframe must be at the start of each keyframe.
-   `fragment_per_frame`: (optional) Each frame should be a separate fragment to minimize latency.

This client can currently be used in conjuction with either ffmpeg or gstreamer.

### ffmpeg

moq-pub can be run as a binary, accepting a stream (from ffmpeg via stdin) and publishing it to the given relay.
See [dev/pub](dev/pub) for the required ffmpeg flags.

### gstreamer

moq-pub can also be run as a library, currently used for a [gstreamer plugin](https://github.com/kixelated/moq-gst).
This is in a separate repository to avoid gstreamer being a hard requirement.
See [run](https://github.com/kixelated/moq-gst/blob/main/run) for an example pipeline.

## moq-transport

A media-agnostic library used by [moq-relay](moq-relay) and [moq-pub](moq-pub) to serve the underlying subscriptions.
It has caching/deduplication built-in, so your application is oblivious to the number of connections under the hood.

See the published [crate](https://crates.io/crates/moq-transport) and [documentation](https://docs.rs/moq-transport/latest/moq_transport/).

## moq-clock

[moq-clock](moq-clock) is a simple client that can publish or subscribe to the current time.
It's meant to demonstate that [moq-transport](moq-transport) can be used for more than just media.

## moq-dir

[moq-dir](moq-dir) is a server that aggregates announcements.
It produces tracks based on the prefix, which are subscribable and can be used to discover broadcasts.

For example, if a client announces the broadcast `.public.room.12345.alice`, then `moq-dir` will produce the following track:

```
TRACK namespace=. track=public.room.12345.
OBJECT +alice
```

Use the `--announce <moq-dir-url>` flag when running the relay to forward all announcements to the instance.

## moq-api

This is a API server that exposes a REST API.
It's used by relays to inserts themselves as origins when publishing, and to find the origin when subscribing.
It's basically just a thin wrapper around redis that is only needed to run multiple relays in a (simple) cluster.

# License

Licensed under either:

-   Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
-   MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)



# MoQ Relay-Side Drop Testing Guide

This guide describes how to set up and test the relay-side drop implementation for Media over QUIC (MoQ).

## Prerequisites

- Ubuntu Linux
- Mininet network emulator
- Rust toolchain (cargo)

Install Mininet:
sudo apt-get install mininet

## Installation

Clone the repository:
git clone --branch relay-side-drop-implementation --single-branch https://github.com/rimmelb/moq-rs.git moq-rs-relay-side-drop
cd moq-rs-relay-side-drop


Build the project:
cargo build --release

Make the scripts executable:
cd dev
chmod +x relay_for_mininet pub_for_mininet sub_for_mininet


## Configuration

### Test Scenarios

Configure the following parameters in the respective scripts before running tests.

#### Scenario 1: Relay-side drop with link information

**In `relay_for_mininet`:**
- `ENABLE_RELAY_DROP=true`
- `ENABLE_LINK_CAPACITY=true`

**In `sub_for_mininet`:**
- `ENABLE_DELIVERY=false`

#### Scenario 2: Relay-side drop without link information

**In `relay_for_mininet`:**
- `ENABLE_RELAY_DROP=true`
- `ENABLE_LINK_CAPACITY=false`

**In `sub_for_mininet`:**
- `ENABLE_DELIVERY=false`

#### Scenario 3: Subscriber-side drop

**In `relay_for_mininet`:**
- `ENABLE_RELAY_DROP=false`
- `ENABLE_LINK_CAPACITY=false`

**In `sub_for_mininet`:**
- `ENABLE_DELIVERY=true`

### Tunable Parameters

#### Relay-side drop parameters
- **Delivery timeout:** Modify `DELIVERY_TIMEOUT` in `relay_for_mininet`
- **Link capacity:** Modify `RELAY_RATE_LIMIT_MBPS` in `relay_for_mininet`

#### Subscriber-side drop parameters
- **Delivery timeout:** Modify `DEADLINE_THRESHOLD_MS` in `sub_for_mininet`

#### Fixed bandwidth testing
For subscriber-side drop testing, set the link capacity between relay and subscriber to the desired fixed bandwidth value.

## Running Tests

First, step into the main directory, then run the following commands:
./dev/relay_for_mininet
./dev/pub_for_mininet
./dev/sub_for_mininet

Navigate to the mininet directory:
cd dev/mininet

### Test 1: Fixed Bandwidth
sudo python3 mininet_test.py

### Test 2: Bandwidth Drop at Half-Time
sudo python3 mininet_halftime_test.py

### Test 3: Dynamic Bandwidth
sudo python3 mininet_real_world_bandwidth_test.py --bandwidth-file /home/user/moq-rs-relay-side-drop/tools/param.txt

**Note:** First-time execution may encounter startup issues. Simply retry if this occurs.

## Evaluation

### Pre-Test Preparation

Before each test, clear the temporary directory:
rm -rf tmp/*

### Post-Test Analysis

After the video stream completes, run the post-processing script:
sudo python3 tools/post_processing.py


### Results

The evaluation results are saved to `tmp/comprehensive_results.json` with the following structure:

{
"qos": {
"vmaf": 98.742497,
"psnr": "Infinity",
"ssim": 1.0
},
"qoe": {
"vmaf": 84.869567,
"psnr": 15.81,
"ssim": 0.731159,
"perceptual_score": 73.46083446430755,
"freeze_events": 1929,
"freeze_duration_sec": 81.018,
"video_duration_sec": 535.878,
"freeze_ratio": 0.1511873971314366
},
"delivery": {
"total_sent": 12759,
"total_received": 10830,
"loss_rate_percent": 15.118739713143665,
"lost_bytes": 21093662,
"sync_time_sec": 2.5
}
}

### Metrics Explanation

- **qos:** Quality of Service metrics measuring technical video quality (not relevant)
- **qoe:** Quality of Experience metrics (primary evaluation metrics)
  - `vmaf`: Video Multi-Method Assessment Fusion score
  - `psnr`: Peak Signal-to-Noise Ratio
  - `ssim`: Structural Similarity Index
  - `perceptual_score`: Overall perceptual quality score
  - `freeze_events`: Number of video freeze occurrences
  - `freeze_duration_sec`: Total duration of freezes
  - `video_duration_sec`: Total video duration
  - `freeze_ratio`: Ratio of freeze time to total duration
- **delivery:** Network delivery statistics

**The primary evaluation results are in the `qoe` section.**

## Troubleshooting

- If scripts fail on first run, retry the command
- Ensure `tmp/` directory exists and is writable
- Verify all scripts have execute permissions
- Check that the bandwidth file path is correct for dynamic tests
