#!/bin/bash

set -e
set -u

KEY=$(cat ~/.config/glcim/config.toml | grep ^api_key | awk -F\" '{print $2}')
GLHOST=$(cat ~/.config/glcim/config.toml | grep ^hostname | awk -F\" '{print $2}')
PROJECT=$(cat ~/.config/glcim/config.toml | grep ^project | awk -F\" '{print $2}')

project_encoded=$(echo ${PROJECT} | sed s@/@%2F@g)

projects() {
    curl -s https://${GLHOST}/api/v4/projects/${project_encoded}/$1\?name=x\&access_token=${KEY} | jq .
}

"$@"
