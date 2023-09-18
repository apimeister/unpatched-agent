# Monitoring Agent

[![codecov](https://codecov.io/gh/apimeister/unpatched-agent/branch/main/graph/badge.svg?token=98HDNPU1IZ)](https://codecov.io/gh/apimeister/unpatched-agent)

Agent for [Unpatch Server](https://github.com/apimeister/unpatched-server)

## usage

```shell
A bash first monitoring solution

Usage: unpatched-agent [OPTIONS] --alias <ALIAS> --id <ID>

Options:
  -s, --server <SERVER>          host:port with unpatched server running [default: 127.0.0.1:3000]
  -a, --alias <ALIAS>            this agents name
      --id <ID>                  this agents id (get from server)
      --attributes <ATTRIBUTES>  attributes describing the server
      --no-tls                   deactivate tls for frontend
  -h, --help                     Print help
  -V, --version                  Print version
```

## TLS

All root certificates will be auto accepted independent from issuer
This applies to web certificates and self-signed certificates
