#!/bin/bash

set -e
set -u

KEY=$(cat ~/.config/glim/config.toml | grep ^api_key | awk -F\" '{print $2}')
GLHOST=$(cat ~/.config/glim/config.toml | grep ^hostname | awk -F\" '{print $2}')
PROJECT=$(cat ~/.config/glim/config.toml | grep ^project | awk -F\" '{print $2}')

project_encoded=$(echo ${PROJECT} | sed s@/@%2F@g)

projects() {
    curl -s https://${GLHOST}/api/v4/projects/${project_encoded}/$1\?access_token=${KEY} | jq .
}

job-log() {
    curl https://${GLHOST}/api/v4/projects/${project_encoded}/jobs/$1/trace\?access_token=${KEY} -L -o output.txt -C -
}


"$@"
