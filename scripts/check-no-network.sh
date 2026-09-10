#!/bin/sh
set -eu

if grep -R -n -E 'Tcp(Stream|Listener)|UdpSocket|reqwest|hyper::|ureq::|curl::' src Cargo.toml
then
    echo 'product source or direct dependencies expose a network-access path' >&2
    exit 1
fi

echo 'product binary source has no network-access path'
