#!/bin/bash

set -e

EXENAME=glcim
dest=bin/${EXENAME}

while [[ $# != 0 ]] ; do
    if [[ "$1" == "--dest" ]] ; then
	shift
        dest=$1
        shift
        continue
    fi
    break
done

T=/tmp/$USER/rust/targets/`pwd`/target

mkdir -p $T
cargo build --release --target-dir ${T}

mkdir -p bin/
rm -f ${dest}
cp $T/release/${EXENAME} ${dest}
