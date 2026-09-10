#!/usr/bin/env bash
# Publishes the workspace to crates.io, and can be run a second time to finish what a first run
# started.
#
# `cargo publish --workspace` works out the dependency order itself, waits for the index between
# crates, and is what does the work here. The two things it does not do are why this script exists.
# crates.io rate limits publishing, so a workspace this size gets a 429 partway through, and a
# second call after one of those stops at the first crate that is already up rather than carrying
# on past it. So this asks the index what is already there, excludes it, and waits when the
# registry tells it to wait.
#
# There are two rate limits and the difference between them is three hours.
#
#   A new crate:            a burst of 5, then one every ten minutes.
#   A new version of one:   a burst of 30, then one a minute.
#
# So the first release of this workspace is 27 new crates and takes about three and a half hours of
# mostly waiting, and every release after it is a couple of minutes. The pause is picked from which
# limit is actually in play rather than being one number that is either wrong or slow, and the
# attempt count is picked from how many crates are left. Nothing here is clever; it is just the
# arithmetic of the published limits, written down so the next person does not have to rediscover
# it at three in the morning during a release.
#
# Run from the root of the workspace with CARGO_REGISTRY_TOKEN set. Running it when everything is
# already published is a no-op that exits zero, which is what makes re-running the release job the
# way to recover from a partial upload.

set -euo pipefail

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
  # This is how the 0.0.1 release failed: the secret was never set on the repository, the binaries
  # built and attested and shipped, and the last job died on "please provide a non-empty token"
  # after twenty three minutes. Failing on the first line with the reason is better.
  echo "CARGO_REGISTRY_TOKEN is empty, so nothing can be published" >&2
  echo "set it at https://github.com/tamnd/rudb/settings/secrets/actions" >&2
  exit 1
fi

# Ten minutes and ten seconds, which is the new crate limit plus enough slack that a clock
# disagreement between here and the registry does not cost a whole extra round.
new_pause=610
# Seventy seconds, likewise, for the one a minute limit on a crate that already exists.
existing_pause=70

metadata=$(cargo metadata --format-version 1 --no-deps)
version=$(echo "$metadata" | jq -r '.packages[] | select(.name == "rudb") | .version')
# `publish = false` is how a crate says it is not for the registry, which is what `xtask` says.
crates=$(echo "$metadata" | jq -r '.packages[] | select(.publish != []) | .name' | sort)
total=$(echo "$crates" | wc -w | tr -d ' ')

# Where a crate lives in the sparse index, which is by the length of its name and is the same rule
# every registry client implements.
index_path() {
  local name=$1
  case ${#name} in
    1) echo "1/$name" ;;
    2) echo "2/$name" ;;
    3) echo "3/${name:0:1}/$name" ;;
    *) echo "${name:0:2}/${name:2:2}/$name" ;;
  esac
}

# The index is asked rather than the API, because the index is what cargo itself reads and a crate
# that has never been published is a 404 there rather than an empty answer. Two questions are asked
# of it: whether this version is up, and whether the crate exists at all, because the second is
# what decides which rate limit applies.
index_entry() {
  curl --silent --fail "https://index.crates.io/$(index_path "$1")" 2>/dev/null || true
}

# One attempt per crate still to do, plus a few. Uploading is what makes progress, so an attempt
# that publishes nothing at all is the only kind worth budgeting against, and there is no shape of
# rate limit where more than one of those happens in a row.
remaining=$total
attempts=$((total + 5))

for attempt in $(seq 1 "$attempts"); do
  exclude=()
  up=0
  brand_new=0
  for crate in $crates; do
    entry=$(index_entry "$crate")
    if echo "$entry" | grep -q "\"vers\":\"$version\""; then
      exclude+=(--exclude "$crate")
      up=$((up + 1))
    elif [ -z "$entry" ]; then
      brand_new=$((brand_new + 1))
    fi
  done

  if [ "$up" -eq "$total" ]; then
    echo "all $total crates are on crates.io at $version"
    exit 0
  fi

  remaining=$((total - up))
  if [ "$brand_new" -gt 0 ]; then
    pause=$new_pause
    echo "attempt $attempt: $up of $total up, $remaining to go, $brand_new of them never published"
    echo "  the new crate limit is one every ten minutes, so this will take a while"
  else
    pause=$existing_pause
    echo "attempt $attempt: $up of $total up, $remaining to go, all of them existing crates"
  fi

  if cargo publish --workspace --locked "${exclude[@]}" 2>&1 | tee /tmp/publish.log; then
    echo "published $remaining crates at $version"
    exit 0
  fi

  if ! grep -q "429 Too Many Requests" /tmp/publish.log; then
    echo "the publish failed for a reason that waiting will not fix" >&2
    exit 1
  fi
  echo "crates.io asked for a slower pace, waiting ${pause}s and carrying on where it stopped"
  sleep "$pause"
done

echo "gave up after $attempts attempts, $((total - remaining)) of $total crates are up at $version" >&2
echo "nothing is lost: re-run this job and it will carry on from here" >&2
exit 1
