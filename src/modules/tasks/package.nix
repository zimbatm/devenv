# Resolves the devenv-tasks binary using devenv's own locked nixpkgs,
# regardless of the user's nixpkgs. This ensures the store path matches
# what CI builds and pushes to devenv.cachix.org.
#
# The locked sources come from builtins.fetchTree, not fetchFromGitHub: a
# fetcher derivation would make the import below an import from derivation,
# which evaluators without IFD (iets, or Nix with
# allow-import-from-derivation = false) refuse.
{ pkgs, lib }:
let
  lock = builtins.fromJSON (builtins.readFile ./../../../flake.lock);
  nixpkgsNodeName = lock.nodes.root.inputs.nixpkgs;
  lockedNixpkgs = lock.nodes.${nixpkgsNodeName}.locked;
  lockedRustOverlay = lock.nodes.rust-overlay.locked or null;

  fetchLocked = locked: (builtins.fetchTree locked).outPath;

  rustOverlaySource =
    if lockedRustOverlay != null && lockedRustOverlay.type == "github" then
      fetchLocked lockedRustOverlay
    else
      null;

  devenvPkgs =
    if lockedNixpkgs.type == "github" then
      let
        rustOverlay = if rustOverlaySource != null then import rustOverlaySource else null;
        overlays = lib.optional (rustOverlay != null) rustOverlay;
      in
      import (fetchLocked lockedNixpkgs) { inherit overlays; system = pkgs.stdenv.system; }
    else
      builtins.trace
        "warning: devenv-tasks: could not resolve devenv's nixpkgs from flake.lock (type=${lockedNixpkgs.type}); falling back to user's nixpkgs. The resulting store path may not match the cached binary."
        pkgs;

  rustToolchain =
    if devenvPkgs ? rust-bin
    then devenvPkgs.rust-bin.stable.latest.default
    else devenvPkgs.rustc;

  workspace = devenvPkgs.callPackage ./../../../nix/workspace.nix {
    rustc = rustToolchain;
    cargo = rustToolchain;
  };
in
workspace.crates.devenv-tasks
