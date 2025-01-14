#!/usr/bin/env bash
set -eu

parent_path=$( cd "$(dirname "${BASH_SOURCE[0]}")" ; pwd -P )

cd "${parent_path}/../"

die () {
    echo >&2 "$@"
    exit 1
}

[ "$#" -eq 2 ] || die "build path and testnet chain ID arguments required"


build_path="$1"
testnet_chain_id="$2"
if [ -d "${build_path}" ] 
then
    printf "Directory ${build_path} already exists. Please remove it if you really want to DELETE ALL YOUR VALIDATOR DATA AND START OVER." >&2
    printf "\n\nIf you just want to rebuild the Docker containers to reflect code changes and maintain the tendermint and penumbra database state, try:\n" >&2
    printf "\t\`docker-compose up --build -d\`" >&2
    exit 1
fi

printf "\t\tA new genesis has occurred...\n\n"
printf "Storing configs to ${build_path}/ ...\n\n\n"

mkdir -p ${build_path}
docker-compose stop
docker container prune
docker volume rm penumbra_tendermint_data || true
docker volume rm penumbra_prometheus_data || true
docker volume rm penumbra_db_data || true

# The db container must be running for pd build to succeed
docker-compose up -d db
# sleep 1 second because postgres isn't immediately responsive
sleep 1
cd pd
printf "Preparing DB\n"
cargo sqlx database create
cargo sqlx migrate run
cargo sqlx prepare  -- --lib
printf "Done\n"
cd ..

python3 -m venv scripts/.venv
source scripts/.venv/bin/activate
pip install -r scripts/requirements.txt
python3 scripts/setup_validator.py ${testnet_chain_id} ${build_path}
