#!/bin/bash

#
# Use with tmux in the following manner:
#
# [hooks]
# open_job_command = "~/.config/glcim/open-job.sh tmux-split"
#

front-program() {
    local project="${GLCIM_PROJECT//\//%2F}"
    while [ 1 ] ; do
	curl\
	    --location --header "PRIVATE-TOKEN: ${GLCIM_API_KEY}" \
	    "https://${GLCIM_HOSTNAME}/api/v4/projects/${project}/jobs/"${GLCIM_JOB_ID}"/trace" \
	   -L -s -o ${1} -C -
	sleep 1
    done
}

tail-program() {
    set -e

    local dir=/tmp/$USER/tail/gitlab-ci-jobs
    local outfile=${dir}/${GLCIM_JOB_ID}

    printf '\033]2;%s\033\\' "${GLCIM_PROJECT} - ${GLCIM_PIPELINE_ID} - job ${GLCIM_JOB_ID} - ${GLCIM_JOB_NAME}"

    mkdir -p ${dir}
    touch ${outfile}

    if [[ "$1" == "-d" ]] ; then
	echo ${outfile}
	front-program ${outfile}
    else
	front-program ${outfile} &
	tail -n 200000 -f ${outfile}

	kill %1
    fi
}

tmux-split() {
    tmux split-window "GLCIM_API_KEY=${GLCIM_API_KEY} GLCIM_HOSTNAME=${GLCIM_HOSTNAME} GLCIM_PIPELINE_ID=${GLCIM_PIPELINE_ID} GLCIM_JOB_ID=${GLCIM_JOB_ID} GLCIM_JOB_NAME=${GLCIM_JOB_NAME} GLCIM_PROJECT=${GLCIM_PROJECT} ${BASH_SOURCE} tail-program"
}

"$@"
