#!/bin/sh

# Define the start command
START_COMMAND="/usr/local/bin/sim-cli $SIMFILE_PATH"

# Check for version arg
if [[ ! -z ${VERSION} ]]; then
    START_COMMAND="$START_COMMAND --version"
fi

# Check for help arg
if [[ ! -z ${HELP} ]]; then
    START_COMMAND="$START_COMMAND --help"
fi

# Check for log level arg
if [[ ! -z ${LOG_LEVEL} ]]; then
    START_COMMAND="$START_COMMAND --log-level $LOG_LEVEL"
fi

# Check for total time arg
if [[ ! -z ${TOTAL_TIME} ]]; then
    START_COMMAND="$START_COMMAND --total-time $TOTAL_TIME"
fi

# Check for print-batch-size arg
if [[ ! -z ${PRINT_BATCH_SIZE} ]]; then
    START_COMMAND="$START_COMMAND --print-batch-size $PRINT_BATCH_SIZE"
fi

# start the container
exec $START_COMMAND