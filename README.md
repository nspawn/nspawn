# nspawn
A wrapper around machinectl for easy-deployment of [https://nspawn.org](https://nspawn.org) images.

## How to use it
```
nspawn {COMMAND} [PARAMETER]

Wrapper around systemd-machined and https://nspawn.org

Commands:
  --init          	Initializes an image for systemd-machined with the following parameters: <distribution>/<release>/<type>
  --list          	Lists all available images
  --help          	Prints this help message

Parameters:
  <distribution>	One out of (archlinux,centos,debian,fedora,ubuntu)
  <release>     	The release of the distribution
  <type>        	One out of (raw,tar)
```

On the first start `nspawn` will try to set up the `/etc/systemd/import-pubring.gpg` keyring.  
`nspawn` will create the keyring and search for the [https://nspawn.org](https://nspawn.org) master key.  
After keyring generation you can start using `nspawn`.

You can use `nspawn --init <distribution>/<release>/<type>` to pull an image.  
`nspawn --list` will print a list of all available images.

## Examples

`nspawn --init fedora/28/tar` will pull a tar archive with a `fedora 28` directory.  
You can instantly start it via `machinectl start fedora-28-tar`.
