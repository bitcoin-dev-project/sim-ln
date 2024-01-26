# sim-ln Docker Setup

This guide provides instructions on how to build, run, and manage the `sim-ln` Docker container using the provided Makefile.

## Prerequisites

- Docker installed on your machine.
- A `sim.json` file that contains your specific configuration.

## 1. Building the Docker Image

To build the Docker image, execute:

```bash
make build-docker
```

This command will make the necessary preparations and build the Docker image.

## 2. Mounting the Volume

To ensure that the Docker container can access the configuration and authentication credentials, we use Docker volumes. Before you can run the container, you need to set up the volume with the authentication credentials.

If you are running the nodes locally, we've provided a convenient script to set up the volume:

```bash
make mount-volume SIMFILE_PATH=/path/to/your/sim.json
```

Replace `/path/to/your/sim.json` with the actual path to your `sim.json` file.

The script will automatically copy your configuration, certificates, macaroons, and other authentication config to a Docker volume named simln-data.
It will also replace `localhost` addresses with `host.docker.internal` so that docker is able to make queries from inside of the container.

*Note:* The path to your config must be an absolute path not relative e.g. "/Users/anonymous/bitcoin-dev-project/sim-ln/sim.json"

### For Remote Nodes

If you're running nodes on a remote machine, or if you have a more complex setup, you'll need to manually set up the Docker volume and adjust your sim.json:

1. Create a Docker volume named simln-data.
2. Copy your configuration, certificates, macaroons, etc., to the volume.
3. Adjust the paths in sim.json to point to the appropriate locations within the volume.
4. Ensure that your addresses are reachable from *within* the docker container. 

<details>
  <summary>Tip: Using host.docker.internal</summary>

Docker provides a special DNS name `host.docker.internal` that resolves to the host machine's IP address because we can't `localhost` or `127.0.0.1` for the IP address. 

This value can be used when trying to access things running on the local machine from within a docker container.

For instance, in your configuration:

```json
{
  "id": "022579561f6f0ea86e330df2f9e7b2be3e0a53f8552f9d5293b80dfc1038f2f66d",
  "address": "https://host.docker.internal:10002",
  "macaroon": "/path/in/container/lnd/bob/data/chain/bitcoin/regtest/admin.macaroon",
  "cert": "/path/in/container/lnd/bob/tls.cert"
}
```

In the above example, the `address` field uses `host.docker.internal` to refer to a service running on port `10002` on the host machine. This special DNS name ensures that your containerized application can reach out to services running on the host.
</details>

## 3. Running the Docker Container

To run the `sim-ln` Docker container:

```bash
make run-docker
```

You can adjust the logging level by providing the `LOG_LEVEL` variable. The default value is `info`. Example:

```bash
make run-docker LOG_LEVEL=debug
```

Other configurable variables include:

- `HELP`: Set to `true` to print the help message.
- `PRINT_BATCH_SIZE`: determines the number of payment results that will be written to disk at a time.
- `TOTAL_TIME`: the total runtime for the simulation expressed in seconds.
- `DATA_DIR`: Path to a directory containing simulation files, and where simulation results will be stored (default is the `/data_dir` volume directory).

Example usage:

```bash
make run-docker PRINT_BATCH_SIZE=100 TOTAL_TIME=5000
```

For an interactive session:

```bash
make run-interactive
```

## 4. Stopping the Container

To stop the `sim-ln` Docker container, use:

```bash
make stop-docker
```

## Help

For a list of available Makefile commands and their descriptions, run:

```bash
make help
```

---

## Notes for Developers:

- When you use the `mount-volume` command, it sets up a Docker volume named `simln-data`. This volume contains your `sim.json` file and other necessary files, ensuring that the data is accessible inside the Docker container.
- For the node address config, docker provides a special DNS name `host.docker.internal` that resolves to the host machine's IP address because we can't `localhost` or `127.0.0.1` for the IP address.
- The paths to certificates, macaroons, etc., in your `sim.json` are automatically adjusted to point to their corresponding paths inside the Docker volume.
