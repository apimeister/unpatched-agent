# Monitoring Agent

[![codecov](https://codecov.io/gh/apimeister/unpatched-agent/branch/main/graph/badge.svg?token=98HDNPU1IZ)](https://codecov.io/gh/apimeister/unpatched-agent)

## usage

```shell
A bash first monitoring solution

Usage: cargo run -- --alias <ALIAS> [--options]

Options:
  -s, --server <SERVER>          host:port with unpatched server running [default: 127.0.0.1:3000]
  -a, --alias <ALIAS>            this agents name
      --attributes <ATTRIBUTES>  attributes describing the server
      --no-tls                   deactivate tls
  -h, --help                     Print help
  -V, --version                  Print version
```

## TLS

### Web Certificates

no action needed from your side

### Self-signed Certificates

#### Option A

add your self-signed rootCA to the CA store [More Info](https://ubuntu.com/server/docs/security-trust-store)

```shell
sudo apt-get install -y ca-certificates
sudo cp rootCA.crt /usr/local/share/ca-certificates
sudo update-ca-certificates
```

#### Option B

use the SSL_CERT_FILE env variable to link to your self-signed rootCA

```shell
# example
SSL_CERT_FILE=./rootCA.crt cargo run -- --alias cool-agent-333 --attributes linux,prod
```
