# Conduit for Nix/NixOS

This guide assumes you have a recent version of Nix (^2.4) installed.

Since Conduit ships as a Nix flake, you'll first need to [enable
flakes][enable_flakes].

You can now use the usual Nix commands to interact with Conduit's flake. For
example, `nix run gitlab:famedly/conduit` will run Conduit (though you'll need
to provide configuration and such manually as usual).

If your NixOS configuration is defined as a flake, you can depend on this flake
to provide a more up-to-date version than provided by `nixpkgs`. In your flake,
add the following to your `inputs`:

```nix
conduit = {
    url = "gitlab:famedly/conduit";

    # Assuming you have an input for nixpkgs called `nixpkgs`. If you experience
    # build failures while using this, try commenting/deleting this line. This
    # will probably also require you to always build from source.
    inputs.nixpkgs.follows = "nixpkgs";
};
```

Next, make sure you're passing your flake inputs to the `specialArgs` argument
of `nixpkgs.lib.nixosSystem` [as explained here][specialargs]. This guide will
assume you've named the group `flake-inputs`.

Now you can configure Conduit and a reverse proxy for it. Add the following to
a new Nix file and include it in your configuration:

```nix
{ config
, pkgs
, flake-inputs
, ...
}:

let
  # You'll need to edit these values

  # The hostname that will appear in your user and room IDs
  server_name = "example.com";

  # The hostname that Conduit actually runs on
  #
  # This can be the same as `server_name` if you want. This is only necessary
  # when Conduit is running on a different machine than the one hosting your
  # root domain. This configuration also assumes this is all running on a single
  # machine, some tweaks will need to be made if this is not the case.
  matrix_hostname = "matrix.${server_name}";

  # An admin email for TLS certificate notifications
  admin_email = "admin@${server_name}";

  # These ones you can leave alone

  # Build a dervation that stores the content of `${server_name}/.well-known/matrix/server`
  well_known_server = pkgs.writeText "well-known-matrix-server" ''
    {
      "m.server": "${matrix_hostname}"
    }
  '';

  # Build a dervation that stores the content of `${server_name}/.well-known/matrix/client`
  well_known_client = pkgs.writeText "well-known-matrix-client" ''
    {
      "m.homeserver": {
        "base_url": "https://${matrix_hostname}"
      }
    }
  '';
in

{
  # Configure Conduit itself
  services.matrix-conduit = {
    enable = true;

    # This causes NixOS to use the flake defined in this repository instead of
    # the build of Conduit built into nixpkgs.
    package = flake-inputs.conduit.packages.${pkgs.system}.default;

    settings.global = {
      inherit server_name;
    };
  };

  # Configure automated TLS acquisition/renewal
  security.acme = {
    acceptTerms = true;
    defaults = {
      email = admin_email;
    };
  };

  # ACME data must be readable by the NGINX user
  users.users.nginx.extraGroups = [
    "acme"
  ];

  # Configure NGINX as a reverse proxy
  services.nginx = {
    enable = true;
    recommendedProxySettings = true;

    virtualHosts = {
      "${matrix_hostname}" = {
        forceSSL = true;
        enableACME = true;

        listen = [
          {
            addr = "0.0.0.0";
            port = 443;
            ssl = true;
          }
          {
            addr = "[::]";
            port = 443;
            ssl = true;
          }          {
            addr = "0.0.0.0";
            port = 8448;
            ssl = true;
          }
          {
            addr = "[::]";
            port = 8448;
            ssl = true;
          }
        ];

        locations."/_matrix/" = {
          proxyPass = "http://backend_conduit$request_uri";
          proxyWebsockets = true;
          extraConfig = ''
            proxy_set_header Host $host;
            proxy_buffering off;
          '';
        };

        extraConfig = ''
          merge_slashes off;
        '';
      };

      "${server_name}" = {
        forceSSL = true;
        enableACME = true;

        locations."=/.well-known/matrix/server" = {
          # Use the contents of the derivation built previously
          alias = "${well_known_server}";

          extraConfig = ''
            # Set the header since by default NGINX thinks it's just bytes
            default_type application/json;
          '';
        };

        locations."=/.well-known/matrix/client" = {
          # Use the contents of the derivation built previously
          alias = "${well_known_client}";

          extraConfig = ''
            # Set the header since by default NGINX thinks it's just bytes
            default_type application/json;

            # https://matrix.org/docs/spec/client_server/r0.4.0#web-browser-clients
            add_header Access-Control-Allow-Origin "*";
          '';
        };
      };
    };

    upstreams = {
      "backend_conduit" = {
        servers = {
          "[::1]:${toString config.services.matrix-conduit.settings.global.port}" = { };
        };
      };
    };
  };

  # Open firewall ports for HTTP, HTTPS, and Matrix federation
  networking.firewall.allowedTCPPorts = [ 80 443 8448 ];
  networking.firewall.allowedUDPPorts = [ 80 443 8448 ];
}
```

Now you can rebuild your system configuration and you should be good to go!

[enable_flakes]: https://nixos.wiki/wiki/Flakes#Enable_flakes

[specialargs]: https://nixos.wiki/wiki/Flakes#Using_nix_flakes_with_NixOS
