#!/bin/bash

#
# Use with tmux in the following manner:
#
# [hooks]
# open_job_command = "~/.config/glim/job-mgmt.sh tmux-split"
#

front-program() {
    local project="${GLCIM_PROJECT//\//%2F}"
    set +e

    #
    # We use session cookie instead of API key because the lack of byte range
    # support.
    #
    # See issue: https://gitlab.com/gitlab-org/gitlab/-/issues/216738
    #

    local curl_params=""
    if [[ "${GLCIM_CERT_INSECURE}" == "true" ]] ; then
	curl_params="-k"
    fi

    while [ 1 ] ; do
	if [[ "${GLCIM_COOKIE}" != "" ]] ; then
	    curl \
		${curl_params} --header "Cookie: _gitlab_session="$GLCIM_COOKIE \
		https://${GLCIM_HOSTNAME}/${GLCIM_PROJECT}/-/jobs/${GLCIM_JOB_ID}/raw \
		-L -s -o ${1} -C -
	else
	    curl \
		${curl_params} --header "PRIVATE-TOKEN: ${GLCIM_API_KEY}" \
		"https://${GLCIM_HOSTNAME}/api/v4/projects/${project}/jobs/"${GLCIM_JOB_ID}"/trace" \
		-L -s -o ${1} -C -
	fi
	sleep 1
    done
}

retry-job() {
    :
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
    elif [[ "$1" == "-l" ]] ; then
	front-program ${outfile} &
	less +F -R ${outfile}
	kill %1 2>/dev/null
    else
	front-program ${outfile} &
	tail -n 200000 -f ${outfile}
	kill %1
    fi
}

tail-pager() {
    clear
    tail-program -l
}

tmux-split() {
    local params="GLCIM_HOSTNAME=${GLCIM_HOSTNAME} GLCIM_PIPELINE_ID=${GLCIM_PIPELINE_ID} GLCIM_CERT_INSECURE=${GLCIM_CERT_INSECURE} GLCIM_JOB_ID=${GLCIM_JOB_ID} GLCIM_PROJECT=${GLCIM_PROJECT}"

    set +e

    while read -r pane_id other ; do
	echo $other | grep -q "$params"
	if [[ $? == 0 ]] ; then
	    # Use existing window
	    tmux select-pane -t $pane_id
	    exit 0
	fi
    done < <(tmux list-panes -F '#{pane_id} #{pane_start_command}')

    # Splitting a new window
    tmux split-window -P "GLCIM_API_KEY=${GLCIM_API_KEY} ${params} GLCIM_COOKIE=${GLCIM_COOKIE} GLCIM_CERT_INSECURE=${GLCIM_CERT_INSECURE} GLCIM_JOB_NAME=\"${GLCIM_JOB_NAME}\" ${BASH_SOURCE} tail-program"
}

"$@"
