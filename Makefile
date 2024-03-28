LOG_LEVEL ?= info
DEFAULT_DATA_DIR ?= /data_dir
DEFAULT_SIMFILE_PATH ?= /data_dir/sim.json

build-docker:
	docker build -f docker/Dockerfile -t sim-ln .

mount-volume:
	chmod +x ./docker/setup-volume.sh && ./docker/setup-volume.sh "$(SIMFILE_PATH)"

help:
	@echo "Makefile commands:"
	@echo "build-docker      Builds the Docker image."
	@echo "mount-volume      Mounts the specified volume."
	@echo "run               Runs the Docker container."
	@echo "run-interactive   Runs the Docker container in interactive mode."
	@echo "stop              Stops the Docker container."
	@echo ""
	@echo "Variables:"
	@echo "SIMFILE_PATH      Path to the sim.json file."
	@echo "LOG_LEVEL         Set the logging level (default: info) e.g. <make run LOG_LEVEL=debug>."
	@echo "HELP              Set to true to print the help message (default: false) e.g. <make run HELP=true>."
	@echo "PRINT_BATCH_SIZE  Set the batch size for printing the results e.g. <make run PRINT_BATCH_SIZE=100>."
	@echo "TOTAL_TIME        Set the total time for the simulation e.g. <make run TOTAL_TIME=1000>."
	@echo "DATA_DIR          Set the data directory for the simulation containing simulation files and results e.g. <make run-docker DATA_DIR="/Users/anon/data_dir>"."

run-docker:
	docker run -d --rm --name sim-ln --init -v simln-data:${DEFAULT_DATA_DIR} -e SIMFILE_PATH=${DEFAULT_SIMFILE_PATH} -e LOG_LEVEL=$(LOG_LEVEL) -e HELP=${HELP} -e PRINT_BATCH_SIZE=${PRINT_BATCH_SIZE} -e TOTAL_TIME=${TOTAL_TIME} sim-ln

run-interactive:
	docker run -it --rm --name sim-ln --init -v simln-data:${DEFAULT_DATA_DIR} -e SIMFILE_PATH=${DEFAULT_SIMFILE_PATH} -e DATA_DIR=${DATA_DIR} -e LOG_LEVEL=$(LOG_LEVEL) -e HELP=${HELP} -e PRINT_BATCH_SIZE=${PRINT_BATCH_SIZE} -e TOTAL_TIME=${TOTAL_TIME} sim-ln

stop-docker:
	docker stop sim-ln

check-code:
	cargo fmt --verbose --check --all -- --color always
	cargo clippy --all-features --all-targets --color always -- -D warnings

stable-output:
	@if [ -n "$$(git status --porcelain)" ]; then \
    	echo "Error: There are unstaged or uncommitted changes after running 'make check-code'."; \
    	exit 1; \
	else \
		echo "No unstaged or uncommitted changes found."; \
	fi

check: check-code stable-output
