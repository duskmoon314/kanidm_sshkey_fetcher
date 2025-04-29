# kanidm_sshkey_fetcher

A simple binary to fetch SSH keys for multiple users from a Kanidm server, based on the MPL-2.0 licensed [`kanidm_ssh_authorizedkeys_direct`](https://github.com/kanidm/kanidm/blob/ff6e97164f3ac3ff2b5da123d29f7488bb0d8862/tools/cli/src/ssh_authorizedkeys.rs)

## Usage

The binary simply fetches all SSH keys for the given users and prints them to stdout. It can be used in a pipeline or redirected to a file.

```console
$ kanidm_sshkey_fetcher -h
Fetch SSH keys for multiple users from a Kanidm server

Usage: kanidm_sshkey_fetcher [OPTIONS] [ACCOUNT_IDS]...

Arguments:
  [ACCOUNT_IDS]...  The account ids to fetch, space separated

Options:
  -d, --debug                 
  -H, --url <ADDR>            The address of the kanidm server to connect to
  -C, --ca <CA_PATH>          The certificate file to use
  -c, --config <CONFIG_PATH>  The configuration file to use
  -h, --help                  Print help
  -V, --version               Print version


$ kanidm_sshkey_fetcher -H <kanidm_server_domain> <username0> <username1> ...
ssh-ed25519 ...
ssh-ed25519 ...
```

The configuration file is similar to cli arguments:

```toml
debug = false
addr = "<kanidm_server_domain>"
ca_path = "<path_to_ca_cert>"
account_ids = ["<username0>", "<username1>", ...]
```

### sshd with `AuthorizedKeysCommand`

The binary can be used with `sshd` as the secondary source of SSH keys. This is done by using the `AuthorizedKeysCommand` option in the `sshd_config` file.

```text
# /etc/ssh/sshd_config
AuthorizedKeysCommand /path/to/kanidm_sshkey_fetcher -H <kanidm_server_domain> <username0> <username1> ...
AuthorizedKeysCommandUser nobody
```

In this case, sshd will call the binary to find public keys if the key is not found in the `AuthorizedKeysFile`.

To fetch keys for dynamic users, the configuration file can be used to specify the `account_ids` to fetch. The binary will then fetch the keys for the specified users and print them to stdout.

```text
# /etc/ssh/sshd_config
AuthorizedKeysCommand /path/to/kanidm_sshkey_fetcher -c /path/to/config.toml
AuthorizedKeysCommandUser nobody
```
