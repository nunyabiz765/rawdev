#!/usr/bin/env bash
#
# fetch-samples.sh
#
# Downloads a small RAW-developer test matrix (one sample each: NEF, ARW,
# RAF/X-Trans, ORF, RW2, PEF, non-Pentax DNG, older CR2, + one portrait-
# orientation DNG) from raw.pixls.us.
#
# All files on raw.pixls.us are released CC0 (public domain) by their
# submitters. We do NOT redistribute any of these files in this repo —
# this script only fetches them into a local, git-ignored scratch dir.
#
# Usage: tools/fetch-samples.sh [DEST_DIR]
#   DEST_DIR defaults to "samples" (relative to cwd).
#
set -euo pipefail

DEST="${1:-samples}"
mkdir -p "$DEST"

UA="Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/120.0"

# sha256  url  filename
SAMPLES="$(cat <<'LISTEOF'
639945753240005d80488c0462c01e73a7ab82c96d3d1cdb6661f02e1b3c1319  https://raw.pixls.us/getfile.php/958/nice/Nikon%20-%20D7000%20-%2014bit%2014bit%20compressed%20(Lossless)%20(3:2).NEF  nikon_d7000.nef
04eacd403d92e0c90da9d91b87615af8eda1afc963350d3ab774557c36763eef  https://raw.pixls.us/getfile.php/2271/nice/Sony%20-%20DSLR-A200%20-%2012bit%2012bit%20compressed%20(3:2).ARW  sony_dslr-a200.arw
f756ad5be551a8b3e1b503a8a6f7e8e637fba519c0b0f8c79dd58e94b0287602  https://raw.pixls.us/getfile.php/865/nice/Fujifilm%20-%20X-T2%20-%2014bit%2014bit%20compressed%20(3:2).RAF  fujifilm_x-t2.raf
d7f6ac99ee1f824e81ec0e71599f0f87f9b2e2675f7ddfdb21202281b8bdc5e0  https://raw.pixls.us/getfile.php/1331/nice/Olympus%20-%20E-M10%20-%2016bit%20(4:3).ORF  olympus_e-m10.orf
b4691f8f5ff86e4141eb5ac67988f769f87e32ddfca931c49848c5fd4dd0315c  https://raw.pixls.us/getfile.php/2546/nice/Panasonic%20-%20DMC-GX85%20-%203:2.RW2  panasonic_dmc-gx85.rw2
b4adf98edf20c98222b80fe6a6523d7a65bdb7a704360351c3b2c0a0317bb4de  https://raw.pixls.us/getfile.php/828/nice/Pentax%20-%20K-7%20-%2012bit%20(3:2).PEF  pentax_k-7.pef
bc8e00ccfd8e4753e8ee0735255bff4dcca5fb8d1a1352adfa59b86806d03f44  https://raw.pixls.us/getfile.php/1955/nice/Ricoh%20-%20GR%20II%20-%2012bit%20(3:2).DNG  ricoh_gr-ii.dng
81c5ee6da45c4e22584c5344807ac32e2c9ba82ad274486ba64745ef8dd4f69c  https://raw.pixls.us/getfile.php/1182/nice/Canon%20-%20EOS%205D%20Mark%20II%20-%20RAW%20(3:2).CR2  canon_eos-5d-mark-ii.cr2
7b0327cf015270ec98627ed1dee02ff39a47d1a6d6ff1ecfe164586e7dc7a749  https://raw.pixls.us/getfile.php/1815/nice/Samsung%20-%20SM-G950U%20-%2016bit%20(16:9).dng  samsung_sm-g950u_portrait.dng
LISTEOF
)"

while IFS= read -r line; do
    [ -z "$line" ] && continue
    sha="$(awk '{print $1}' <<<"$line")"
    url="$(awk '{print $2}' <<<"$line")"
    fname="$(awk '{print $3}' <<<"$line")"
    dest_path="$DEST/$fname"

    if [ -f "$dest_path" ]; then
        got="$(sha256sum "$dest_path" | awk '{print $1}')"
        if [ "$got" = "$sha" ]; then
            echo "skip (already verified): $fname"
            continue
        fi
        echo "present but checksum mismatch, re-downloading: $fname"
    fi

    echo "downloading: $fname"
    curl -sL -A "$UA" -o "$dest_path" "$url"

    got="$(sha256sum "$dest_path" | awk '{print $1}')"
    if [ "$got" != "$sha" ]; then
        echo "ERROR: checksum mismatch for $fname (expected $sha, got $got)" >&2
        exit 1
    fi
    echo "verified: $fname"
done <<<"$SAMPLES"

echo "done. samples in: $DEST"
