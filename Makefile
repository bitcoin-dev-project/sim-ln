LOG_LEVEL ?= info

build-docker:
	chmod +x ./docker/build.sh && ./docker/build.sh

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
	@echo "SIMFILE_PATH       Path to the sim.json file."
	@echo "LOG_LEVEL         Set the logging level (default: info) e.g. <make run LOG_LEVEL=debug>"
	@echo "HELP              Set to true to print the help message (default: false) e.g. <make run HELP=true>"
	@echo "PRINT_BATCH_SIZE  Set the batch size for printing the results e.g. <make run PRINT_BATCH_SIZE=100>"
	@echo "TOTAL_TIME        Set the total time for the simulation e.g. <make run TOTAL_TIME=1000>"

run-docker:
	docker run -d --rm --name sim-ln --init -v simln-data:/data -e SIMFILE_PATH=/data/sim.json -e LOG_LEVEL=$(LOG_LEVEL) -e HELP=${HELP} -e PRINT_BATCH_SIZE=${PRINT_BATCH_SIZE} -e TOTAL_TIME=${TOTAL_TIME} sim-ln

run-interactive:
	docker run --rm --name sim-ln --init -v simln-data:/data -e SIMFILE_PATH=/data/sim.json -it sim-ln

stop-docker:
	docker stop sim-ln
