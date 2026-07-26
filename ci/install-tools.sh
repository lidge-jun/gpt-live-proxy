#!/usr/bin/env bash
set -euo pipefail

readonly ACTIONLINT_VERSION="1.7.12"
readonly ACTIONLINT_SHA256="8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8"
readonly ACTIONLINT_URL="https://github.com/rhysd/actionlint/releases/download/v${ACTIONLINT_VERSION}/actionlint_${ACTIONLINT_VERSION}_linux_amd64.tar.gz"

readonly GITLEAKS_VERSION="8.30.1"
readonly GITLEAKS_SHA256="551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"
readonly GITLEAKS_URL="https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz"

readonly CARGO_AUDIT_VERSION="0.22.2"
readonly CARGO_AUDIT_SHA256="700c2b240f7fd330c24b675fe429f73a5b676531fcc6300400b2b67f155ba12a"
readonly CARGO_AUDIT_URL="https://static.crates.io/crates/cargo-audit/cargo-audit-${CARGO_AUDIT_VERSION}.crate"

if [[ "$(uname -s)" != "Linux" || "$(uname -m)" != "x86_64" ]]; then
  echo "install-tools.sh supports only Linux x86_64" >&2
  exit 1
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

install_dir="${HOME}/.local/bin"
mkdir -p "${install_dir}"

download_and_verify() {
  local url="$1"
  local expected_sha="$2"
  local output="$3"

  curl --user-agent 'gpt-live-proxy-ci/0.1' \
    --proto '=https' --tlsv1.2 --fail --silent --show-error \
    --location --output "${output}" "${url}"
  printf '%s  %s\n' "${expected_sha}" "${output}" | sha256sum --check --strict
}

actionlint_archive="${tmp_dir}/actionlint.tar.gz"
download_and_verify "${ACTIONLINT_URL}" "${ACTIONLINT_SHA256}" "${actionlint_archive}"
tar --extract --gzip --file "${actionlint_archive}" --directory "${tmp_dir}" actionlint
install -m 0755 "${tmp_dir}/actionlint" "${install_dir}/actionlint"

gitleaks_archive="${tmp_dir}/gitleaks.tar.gz"
download_and_verify "${GITLEAKS_URL}" "${GITLEAKS_SHA256}" "${gitleaks_archive}"
tar --extract --gzip --file "${gitleaks_archive}" --directory "${tmp_dir}" gitleaks
install -m 0755 "${tmp_dir}/gitleaks" "${install_dir}/gitleaks"

cargo_audit_archive="${tmp_dir}/cargo-audit.crate"
download_and_verify "${CARGO_AUDIT_URL}" "${CARGO_AUDIT_SHA256}" "${cargo_audit_archive}"
mkdir "${tmp_dir}/cargo-audit-src"
tar --extract --gzip --file "${cargo_audit_archive}" --directory "${tmp_dir}/cargo-audit-src"
cargo install \
  --path "${tmp_dir}/cargo-audit-src/cargo-audit-${CARGO_AUDIT_VERSION}" \
  --locked \
  --root "${HOME}/.local"

export PATH="${install_dir}:${PATH}"
if [[ -n "${GITHUB_PATH:-}" ]]; then
  printf '%s\n' "${install_dir}" >> "${GITHUB_PATH}"
fi

actionlint -version | grep -F "${ACTIONLINT_VERSION}"
gitleaks version | grep -F "${GITLEAKS_VERSION}"
cargo audit --version | grep -F "cargo-audit ${CARGO_AUDIT_VERSION}"
