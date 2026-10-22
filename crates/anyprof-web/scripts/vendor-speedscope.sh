#!/usr/bin/env bash
# Re-vendor the speedscope static build that `anyprof view` embeds.
#
# Downloads the pinned npm tarball, checks its sha256, drops the example
# profile and the Java Flight Recorder wasm, gzips every remaining asset with
# `gzip -n -9` (no name or timestamp, so reruns give the same bytes with the
# same gzip), and copies the two licenses into vendor/licenses.
#
# Usage: crates/anyprof-web/scripts/vendor-speedscope.sh
# To upgrade, change VERSION and SHA256 below and rerun.
set -euo pipefail

VERSION="1.25.0"
SHA256="2831f1e0d26df914b477e522d78b8485511495a6af81a0b88158f8b48f9e0d03"
URL="https://registry.npmjs.org/speedscope/-/speedscope-${VERSION}.tgz"

crate_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
assets_dir="${crate_dir}/vendor/speedscope"
licenses_dir="${crate_dir}/vendor/licenses"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

curl --proto '=https' --tlsv1.2 -fsSL "${URL}" -o "${work}/speedscope.tgz"

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "${work}/speedscope.tgz" | cut -d' ' -f1)"
else
    actual="$(shasum -a 256 "${work}/speedscope.tgz" | cut -d' ' -f1)"
fi
if [ "${actual}" != "${SHA256}" ]; then
    echo "sha256 mismatch for ${URL}" >&2
    echo "  expected ${SHA256}" >&2
    echo "  actual   ${actual}" >&2
    exit 1
fi

tar -xzf "${work}/speedscope.tgz" -C "${work}"
release="${work}/package/dist/release"
if ! grep -q "^speedscope@${VERSION}$" "${release}/release.txt"; then
    echo "${release}/release.txt does not name speedscope@${VERSION}" >&2
    exit 1
fi

rm -rf "${assets_dir}"
mkdir -p "${assets_dir}" "${licenses_dir}"
for file in "${release}"/*; do
    name="$(basename "${file}")"
    case "${name}" in
        perf-vertx-*.txt | jfrview_bg*.wasm | source-code-pro.LICENSE.md) continue ;;
    esac
    gzip -n -9 -c "${file}" >"${assets_dir}/${name}.gz"
done

cp "${work}/package/LICENSE" "${licenses_dir}/LICENSE-speedscope.txt"
cp "${release}/source-code-pro.LICENSE.md" "${licenses_dir}/LICENSE-source-code-pro.md"

echo "vendored speedscope@${VERSION} into ${assets_dir}"
ls -l "${assets_dir}"
