scope@{ pkgs ? import <nixpkgs> { } }:

pkgs.buildEnv {
  name = "loom-env";
  paths = with pkgs; [
    git
    bash
    direnv
    binutils
    stdenv
    bashInteractive
    docker
    docker-compose
    cacert
    gcc
    cmake
    rustup
    pkg-config
    openssl
    bundler
    docker
    (glibcLocales.override { locales = [ "en_US.UTF-8" ]; })
    remarshal
    snappy
    gnumake
    autoconf
  ];
  passthru = {
    LOCALE_ARCHIVE = "${pkgs.glibcLocales}/lib/locale/locale-archive";
    LC_ALL = "en_US.UTF-8";
    SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    GIT_SSL_CAINFO = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    CURL_CA_BUNDLE = "${pkgs.cacert}/etc/ca-bundle.crt";
    CARGO_TERM_COLOR = "always";
    RUST_BACKTRACE = "1";
  };
}
