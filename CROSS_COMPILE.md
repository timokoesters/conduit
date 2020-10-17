Install docker:

$ sudo apt install docker
$ sudo usermod -aG docker $USER

Then log out and back in.

$ sudo systemctl start docker

$ cargo install cross
$ cross build --target armv7-unknown-linux-musleabihf

The cross-compiled binary is at target/armv7-unknown-linux-musleabihf/release/conduit
