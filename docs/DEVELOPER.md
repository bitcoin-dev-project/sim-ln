# Developer Documentation

## Debugging

SimLN uses [tokio-console](https://github.com/tokio-rs/console) tracing to help track down deadlocks and performance
issues. 

Install and run SimLN in dev mode to enable tracing:
* `make dev-install`
* Run `sim-cli` as usual

Run `tokio-console`:
* Follow [running the console](https://github.com/tokio-rs/console?tab=readme-ov-file#running-the-console) instructions.

Note that you need to start SimLN *before* tokio-console to get output (otherwise the window will be blank).
