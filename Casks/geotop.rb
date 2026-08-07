# Homebrew cask for geotop — kept in the geotop repo itself (no separate tap
# repository).
#
# On each tag release, the `release.yml` workflow bumps `version` + `sha256`
# here in place, committing straight to the default branch with the default
# GITHUB_TOKEN (contents: write), so there is no second repo and no PAT.
#
# Users install with:
#   brew tap ozkanpakdil/geotop https://github.com/ozkanpakdil/geotop
#   brew trust ozkanpakdil/geotop
#   brew install --cask geotop
#
# The explicit tap URL is needed because brew's shorthand `ozkanpakdil/geotop`
# resolves to a `homebrew-geotop` repo by convention, not this one. The
# `brew trust` step is required by current Homebrew, which refuses to load
# casks from non-official taps until they are explicitly trusted.
#
# The cask installs the prebuilt, Apple-signed + notarized universal2 Mach-O
# (arm64 + x86_64) onto PATH, so there is no Gatekeeper "unidentified
# developer" prompt and no need to choose an architecture.

cask "geotop" do
  version "0.1.6"
  sha256 "0359fd6f9056ecf4f4e15778286941d34c29eaaddedb2ad7f400023f84c984c2"

  url "https://github.com/ozkanpakdil/geotop/releases/download/v#{version}/geotop-darwin-universal.tar.gz"
  name "geotop"
  desc "htop-style real-time network & log monitor with live global geolocation map"
  homepage "https://github.com/ozkanpakdil/geotop"

  # The tarball contains a single Mach-O named `geotop`; `binary` symlinks it
  # into the Homebrew bin dir so it lands on PATH.
  binary "geotop"
end