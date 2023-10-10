#!/bin/bash

# Path to the sim.json on the host
SIMFILE_PATH_ON_HOST=$1

# Check if the sim.json path was provided
if [[ -z "$SIMFILE_PATH_ON_HOST" ]]; then
    echo "Error: Path to sim.json must be provided as an argument."
    exit 1
fi

# Check if jq is installed
if ! command -v jq &> /dev/null; then
    echo "Error: jq is not installed. Please install jq to continue."
    exit 1
fi

# Volume Config Preparation
VOLUME_NAME="simln-data"
STAGING_DIR="/tmp/sim-ln-volume"
mkdir -p $STAGING_DIR

# Copy sim.json to a temporary staging directory
cp $SIMFILE_PATH_ON_HOST $STAGING_DIR/sim.json

# Extracting Node Count
NODE_COUNT=$(cat $SIMFILE_PATH_ON_HOST | jq '.nodes | length')

# Loop Over Each Node
for (( i=0; i<$NODE_COUNT; i++ )); do
    NODE_TYPE=$(cat $SIMFILE_PATH_ON_HOST | jq -r ".nodes[$i] | keys[0]") # Determine if it's LND or CLN.
    NODE_ID=$(cat $SIMFILE_PATH_ON_HOST | jq -r ".nodes[$i].$NODE_TYPE.id") # Extract node ID for directory creation.
    NODE_TLS_PATH_ON_HOST=$(cat $SIMFILE_PATH_ON_HOST | jq -r ".nodes[$i].$NODE_TYPE.cert") # TLS path
    NODE_MACAROON_PATH_ON_HOST=$(cat $SIMFILE_PATH_ON_HOST | jq -r ".nodes[$i].$NODE_TYPE.macaroon") # Macaroon path
    
    # Create staging directories for each node
    mkdir -p $STAGING_DIR/lnd/$NODE_ID

    # Copy files to staging directories
    cp $NODE_TLS_PATH_ON_HOST $STAGING_DIR/lnd/$NODE_ID/tls.cert
    cp $NODE_MACAROON_PATH_ON_HOST $STAGING_DIR/lnd/$NODE_ID/admin.macaroon

    # Adjust the paths in the staging sim.json so we don't use the host path
    sed -i 's|'$(dirname $NODE_TLS_PATH_ON_HOST)'/tls.cert|/data/lnd/'$NODE_ID'/tls.cert|' $STAGING_DIR/sim.json
    sed -i 's|'$(dirname $NODE_MACAROON_PATH_ON_HOST)'/admin.macaroon|/data/lnd/'$NODE_ID'/admin.macaroon|' $STAGING_DIR/sim.json
done

# Create Docker volume and copy the data
docker volume create $VOLUME_NAME
docker run --rm -v $VOLUME_NAME:/data -v $STAGING_DIR:/staging alpine sh -c 'cp -r /staging/* /data/'
