# minimap — build a vector-tile map from OpenStreetMap.
#
#   make                       what this is, how it is configured, where it got to
#   make all                   download -> load -> bake -> export
#   make all REGIONS=france    ... for a different extract, beside the first
#   make picardie              the same, spelled as a target; also france
#   make europe                download every European extract (31 GB) and build the continent
#   make serve                 serve the archive that came out
#   make test                  every test, in seconds, needing no data
#   make check                 formatting and clippy, warnings as errors
#   make build                 the two binaries, and where they landed
#   make disk-stat             what is on the disk, by category
#   make clean-pmtiles         delete one category: pbf, duckdb, pmtiles, log, anon, binary
#   make clean                 delete all of them (asks before the downloads)
#
# One directory per kind of thing, so what a directory holds is its name:
#
#   $(PBF)      what was downloaded. Expensive, polite, identical on every
#               rebuild, so only `clean-pbf` (and `clean`, asking) touch it.
#   $(DUCKDB)   the database built from it. Enormous, and pure scaffolding once
#               the archives exist.
#   $(PMTILES)  one archive per layer: the deliverable, and all the server needs.
#   $(LOG)      one file per stage.
#
# Every build has a NAME -- `picardie`, `europe`, `belgium+netherlands` -- and
# everything it writes carries it: $(DUCKDB)/<name>.duckdb,
# $(PMTILES)/<name>.<layer>.pmtiles, $(LOG)/<name>.<stage>.log. So a test build
# of Picardie sits beside the continent in the same three directories and can
# never overwrite it, and `make serve NAME=picardie` picks one.
#
# Each is a variable, which matters because they differ by three orders of
# magnitude. `make all DUCKDB=/mnt/big/duckdb` puts the 154 GB where there is
# room for it without moving the 135 MB off the machine that serves it.

SHELL       := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

# --- configuration ---------------------------------------------------------
# Override any of these on the command line: `make all REGIONS=france`.

# The extracts and the coastline. Deleted only by `clean-pbf`, which asks.
PBF     ?= pbf
# The build database and DuckDB's spill. Deleted by `clean-duckdb`.
DUCKDB  ?= duckdb
# One archive per layer -- what ships. Deleted by `clean-pmtiles`, rebuilt by `export`.
PMTILES ?= pmtiles
# One log per stage. Deleted by `clean-log`.
LOG     ?= log
# Geofabrik extract names, space separated. `make regions` lists them.
REGIONS ?= picardie
# What the build is called. Derived from REGIONS unless given: the regions
# sorted and joined with `+`, or `europe` when REGIONS is empty -- which means
# every extract in $(PBF), and `make europe` is how that directory gets filled.
# Override it when $(PBF) holds something else: `make all REGIONS= NAME=world`.
empty :=
space := $(empty) $(empty)
NAME    ?= $(if $(strip $(REGIONS)),$(subst $(space),+,$(sort $(REGIONS))),europe)
# The zoom rungs and the size thresholds are NOT here. They are what this map is
# rather than how this run is invoked, so they are constants in
# minimap_rs/src/tuning.rs -- ZOOMS, BACKGROUND_MAXZOOM, MIN_PIXELS,
# LANDUSE_PIXELS. Editing that file makes the stages below stale and `make all`
# re-runs them, which is the same guarantee a flag would have given with none of
# the machinery.
# DuckDB's memory budget. Empty means half of this machine's RAM.
MEMORY  ?=
# Concurrent downloads. Geofabrik is free; do not be rude.
JOBS    ?= 3
# Where `make serve` listens.
PORT    ?= 8090
# The anonymity-zone index (see anon/README.md), cut from this build's features
# table by `make anon`. Optional: `make serve` works without it, and enables
# click-for-a-zone in the viewer when it is there. Deleted by `clean-anon`.
ANON    ?= anon/$(NAME).anon-zones.bin
# Which baked tier the servers answer from. 64 is a city block's worth of
# vagueness (~200 m in Paris, ~1 km in open country); the index also carries 16
# and 256, so changing this is a restart, not a re-bake.
ANON_K  ?= 64
# Extra anon-bake flags: --min-footprint 25 drops the sheds and barns that
# inflate a hamlet's building count, --k 16,64,256 picks the tiers.
ANON_FLAGS ?=

# --- derived ---------------------------------------------------------------

# One profile everywhere. `bundled` compiles DuckDB, and a debug build of it is
# a second four-minute compile that serves nothing: the pipeline is only ever
# run optimised, and the tests are fast enough optimised to run before every
# commit. So test, check and build all say --release too, and share one target
# directory with the stages.
CARGO   := cargo
TARGET  ?= target
BIN     := $(TARGET)/release
MINIMAP := $(BIN)/minimap
COMMON  := --name $(NAME) --pbf $(PBF) --duckdb $(DUCKDB) --pmtiles $(PMTILES) \
           $(if $(MEMORY),--memory $(MEMORY),) --jobs $(JOBS)

LAND  := $(PBF)/land-polygons-split-3857.zip
PBFS  := $(patsubst %,$(PBF)/%.osm.pbf,$(REGIONS))
DB    := $(DUCKDB)/$(NAME).duckdb

# What the map *is* -- layers, classes, size thresholds, the SQL derived from
# them. Editing it makes the database and the tiles stale, so the stages depend
# on it and `make all` re-runs what it has to. Editing anything else in
# minimap_rs/ is a change to how the work is done, not to what comes out, so it
# deliberately does not invalidate hours of baking.
TUNING := minimap_rs/src/tuning.rs

# Stamps stand in for the stages that all write into the same $(DB) file, which
# make cannot tell apart by timestamp. The settings that change the *result* are
# in the filenames, so asking for different ones misses the stamp and rebuilds.
#
# The load only cares about the deepest rung: the extractor drops anything too
# small to draw there, so a database loaded for z14 is not a database for z17.
# The bake cares about every rung and about the background cap.
# Export gets one too, because it writes a directory of archives rather than a
# single file -- there is no one output whose timestamp stands for the rest.
#
# Each stamp lives *with the thing it describes*: load and bake write the
# database, export writes the archives. So `rm -rf duckdb/` correctly makes load
# pending again, and there is no way to be left holding a stamp that claims
# something exists when it does not. They no longer carry the settings in their
# names, because there is one set of settings and $(TUNING) is what changes it.
#
# REGIONS has to be part of the load's identity, and a plain prerequisite cannot
# do it: `features` is the union of the extracts, but a country downloaded in
# July is *older* than a stamp written today, so make would see nothing to do and
# leave a Europe request holding a Picardie map. Hashing the set into the name is
# what makes switching regions re-load. An empty REGIONS means "every extract
# present", so that case hashes the directory listing instead. The NAME is in
# there too, because the files the stamps stand for carry it.
REGION_SET := $(if $(strip $(REGIONS)),$(sort $(REGIONS)),$(sort $(notdir $(wildcard $(PBF)/*.osm.pbf))))
REGION_ID  := $(firstword $(shell echo '$(REGION_SET)' | cksum))

LOADED   := $(DUCKDB)/.load-$(NAME)-$(REGION_ID)
BAKED    := $(DUCKDB)/.bake-$(NAME)-$(REGION_ID)
EXPORTED := $(PMTILES)/.export-$(NAME)-$(REGION_ID)

.DEFAULT_GOAL := help
.PHONY: help all download europe load bake export anon serve anon-serve \
        picardie france test check build perf working-set info sql regions disk-stat prune clean adopt dirs targets

# --- the binaries ----------------------------------------------------------
# The stages run the compiled binaries directly rather than through `cargo run
# --quiet`, which hides the one slow compile there is: `bundled` DuckDB, four
# minutes the first time, in silence. Every target that runs a binary depends
# on building it: a no-op in a fraction of a second when nothing changed, and
# cargo's own progress bar the first time. (A source edit after that rebuilds
# quietly; only this crate recompiles then, half a minute at most.)
bin-%:
	@if [ -x $(BIN)/$* ]; then $(CARGO) build --release --quiet --bin $*; \
	 else echo "==> building $*  (the first build compiles DuckDB: ~4 minutes)"; \
	      $(CARGO) build --release --bin $*; fi

# --- the pipeline ----------------------------------------------------------
# Each stage writes its own log via --log, rather than being piped through tee.
# A pipe would make the tool's stdout a non-terminal, which is exactly how it
# decides whether to draw a live progress line -- so `| tee` would silently
# trade the thing you are watching for the file you are not.

all: export

download: $(PBFS) $(LAND)

$(PBF)/%.osm.pbf: | dirs bin-minimap
	@$(MINIMAP) download $(COMMON) $*

$(LAND): | dirs bin-minimap
	@$(MINIMAP) download $(COMMON) --land

# --- the maps, by name -----------------------------------------------------
# One target per map worth having a name for. Each is `make all` with the
# right REGIONS, run as a sub-make so the stamps and the NAME come out exactly
# as they would by hand -- `make picardie` and `make all REGIONS=picardie` are
# the same build.

picardie france:
	@$(MAKE) --no-print-directory all REGIONS=$@

# The continent: fetch every European country extract Geofabrik publishes
# (49 of them, 31 GB, hours -- resumable, so an interrupted run picks up), then
# build from everything in $(PBF). Sub-regions already there, Picardie say, are
# folded in and deduplicated by the load; they cost time, not correctness.
europe: | dirs bin-minimap
	@$(MINIMAP) download $(COMMON) --log $(LOG)/europe.download.log --europe
	@$(MAKE) --no-print-directory all REGIONS= NAME=europe

load: $(LOADED)
$(LOADED): $(PBFS) $(LAND) $(TUNING) | dirs bin-minimap
	@$(MINIMAP) load $(COMMON) --log $(LOG)/$(NAME).load.log $(REGIONS)
	@touch $@

bake: $(BAKED)
$(BAKED): $(LOADED) $(TUNING) | dirs bin-minimap
	@$(MINIMAP) bake $(COMMON) --log $(LOG)/$(NAME).bake.log
	@touch $@

export: $(EXPORTED)
$(EXPORTED): $(BAKED) | dirs bin-minimap
	@$(MINIMAP) export $(COMMON) --log $(LOG)/$(NAME).export.log
	@touch $@

# The anon index is cut from the same `features` table, so it is stale whenever
# the load is -- and whenever the anon code is, which the pipeline stamps do not
# see. Off the `all` path on purpose: it is a second deliverable, not a stage.
anon: $(ANON)
$(ANON): $(LOADED) $(wildcard anon/format/src/*.rs) $(wildcard anon/bake/src/*.rs) | dirs bin-anon-bake
	@$(if $(MEMORY),MINIMAP_MEMORY_LIMIT=$(MEMORY) ,)$(BIN)/anon-bake \
	  --db $(DB) --out $@ --log $(LOG)/$(NAME).anon.log $(ANON_FLAGS)

# --- using the result ------------------------------------------------------

# Demand the pipeline only while the scaffolding to run it is there. After
# `make prune` the database and its stamps are gone; without the guard, make
# would see a missing $(BAKED) and drag a pruned machine into an hours-long
# rebuild just to serve archives it already has. No database means the
# archives are the truth: serve them as they are.
serve: $(if $(wildcard $(DB)),$(EXPORTED),) | bin-minimap-backend
	@[ -n "$$(ls -1 $(PMTILES)/$(NAME).*.pmtiles 2>/dev/null)" ] || { echo "nothing to serve for NAME=$(NAME) -- run: make all"; exit 1; }
	@MINIMAP_TILES=$(PMTILES) MINIMAP_NAME=$(NAME) MINIMAP_PORT=$(PORT) ANON_INDEX=$(ANON) ANON_K=$(ANON_K) \
	  $(BIN)/minimap-backend

# The standalone zone service, for deploying the lookup without the map --
# same index, same answers, none of the tiles. See anon/README.md for the
# proxy configuration it needs in front of it (in short: no request logging).
anon-serve: $(ANON) | bin-anon-serve
	@ANON_INDEX=$(ANON) ANON_K=$(ANON_K) $(BIN)/anon-serve

# --- development -----------------------------------------------------------

# Every unit and integration test in the workspace, none of which needs a
# download or a database: the pipeline's tests use in-memory DuckDB, the
# server's build their own archives. The perf report is the one test skipped
# here -- it prints numbers rather than checking them, and `make perf` is how
# it is read. The viewer's decoder is tested under node when node is present;
# a machine without it skips that and says so. Not --quiet: the compile is
# the slow part the first time, and cargo's bar is the progress it has.
test:
	@$(CARGO) test --release --workspace -- --skip cache_perf
	@if command -v node >/dev/null 2>&1; then node --test server/web/minimap.test.js; \
	 else echo "node not found -- skipping server/web/minimap.test.js"; fi

# What a reviewer would ask for: formatted, and clippy-clean with warnings as
# errors, over every target including the tests and examples.
check:
	@$(CARGO) fmt --all --check
	@$(CARGO) clippy --release --workspace --all-targets -- -D warnings

# Both binaries, and where they are. Deploying the map is `minimap-backend`
# (the viewer is compiled into it) plus $(PMTILES) plus, optionally, $(ANON);
# `minimap` never leaves the build machine.
build:
	@$(CARGO) build --release --workspace --bins
	@for b in minimap minimap-backend anon-bake anon-serve; do \
	    printf "  %-16s %s\n" "$$b" "$$(du -h $(BIN)/$$b | cut -f1)"; done
	@echo "  deploy: target/release/minimap-backend + $(PMTILES)/ + $(ANON) -- see server/README.md"

# What the server's caches cost: leaf-directory hits, etag revalidation, and
# RSS under sustained load. See server/tests/cache_perf.rs.
#
# It builds its own archives, so it needs nothing from the pipeline and runs
# anywhere. TILES points it at real ones as well, which is the only way to see
# what a miss costs when it is a page fault against a 15 GB file instead of a
# gunzip against warm memory -- and it will pull GBs through the page cache.
#
#   make perf
#   make perf SCALE=10             ten times the iterations
#   make perf TILES=$(PMTILES)   ... and the archives that shipped
SCALE ?= 1
TILES ?=
perf:
	@$(if $(TILES),MINIMAP_TILES=$(abspath $(TILES)) ,)MINIMAP_PERF_SCALE=$(SCALE) \
	  $(CARGO) test --release -p minimap-server --test cache_perf -- --nocapture

# How much of the archive has to stay in RAM, given that traffic lands on city
# centres rather than spreading over the map. This is what sizes a box: the
# tiles people look at are a tiny fraction of what shipped, and `perf` above
# says what it costs when they are resident and when they are not.
#
#   make working-set
#   make working-set NAME=picardie                 another build
#   make working-set WHERE="Paris Berlin"          only these
#   make working-set WHERE="35.68,139.69,Tokyo"    a centre the list lacks
WHERE ?=
working-set: | dirs
	@$(CARGO) run --release --quiet -p minimap-server --example working-set -- \
	  $(PMTILES) $(NAME) $(WHERE)

info: | bin-minimap
	@$(MINIMAP) info $(COMMON)

# make sql Q="select layer, count(*) from features group by 1"
#
# Q travels as an environment variable rather than being pasted into the recipe:
# make would expand a multi-line or quote-bearing query straight into the shell
# command, where its newlines end the command early.
export Q
sql: | bin-minimap
	@$(MINIMAP) sql $(COMMON) "$$Q"

regions: | bin-minimap
	@$(MINIMAP) regions $(COMMON)

dirs:
	@mkdir -p $(PBF) $(DUCKDB) $(PMTILES) $(LOG)

# --- disk ------------------------------------------------------------------
# Six things take space, and they cost very different amounts to get back:
#
#   category  what                        recover by
#   pbf       the extracts + coastline    make download -- hours for Europe, and someone else's bandwidth
#   duckdb    the build database + spill  make load -- minutes per region, a day for Europe
#   pmtiles   the archives, one per layer make export (bake first if duckdb is gone too)
#   log       one file per stage          nothing; they are a record
#   anon      the zone index              make anon
#   binary    cargo's target/             the next cargo command; DuckDB alone is ~4 minutes
#
# `disk-stat` says what each holds. `clean-<category>` removes exactly one, and
# nothing else: `clean-pmtiles` leaves the downloads and the compiler output
# where they are. `prune` is what a machine that only serves can drop. `clean`
# is everything. The one that costs hours -- pbf, and so clean -- asks first,
# unless FORCE=1.

DISK_CATEGORIES := pbf duckdb pmtiles log anon binary
DISK_pbf     := $(PBF)
DISK_duckdb  := $(DUCKDB)
DISK_pmtiles := $(PMTILES)
DISK_log     := $(LOG)
# Every build's index, not just this one's: categories are whole directories.
DISK_anon    := anon/*.anon-zones.bin
DISK_binary  := $(TARGET)
RECOVER_pbf     := make download (hours for Europe)
RECOVER_duckdb  := make load
RECOVER_pmtiles := make export
RECOVER_log     := -
RECOVER_anon    := make anon
RECOVER_binary  := any cargo command (~4 min)

# Size of a path (or glob) as `du` prints it, or `-` when there is nothing there.
disk_size = $$(ls -d $(1) >/dev/null 2>&1 && du -shc $(1) 2>/dev/null | tail -1 | cut -f1 || echo -)

disk-stat:
	@printf "  %-9s %8s  %-22s %s\n" category size path "recover by"
	@$(foreach c,$(DISK_CATEGORIES),printf "  %-9s %8s  %-22s %s\n" "$c" "$(call disk_size,$(DISK_$c))" "$(DISK_$c)" "$(RECOVER_$c)";)
	@existing="$(strip $(wildcard $(foreach c,$(DISK_CATEGORIES),$(DISK_$c))))"; \
	 total=$$( [ -n "$$existing" ] && du -shc $$existing 2>/dev/null | tail -1 | cut -f1 || echo 0 ); \
	 printf "  %-9s %8s\n" total "$$total"
	@printf "  %-9s %8s  free on %s\n" disk "$$(df -h . | awk 'NR==2 {print $$4}')" "$$(df -h . | awk 'NR==2 {print $$6}')"
	@echo
	@echo "  make clean-<category> removes one and nothing else; make clean removes all of them"

# One category at a time. Not .PHONY because a pattern cannot be, but no file
# is ever called clean-<x>, so the recipe always runs.
clean-%:
	@p="$(DISK_$*)"; [ -n "$$p" ] || { echo "no such category '$*' -- one of: $(DISK_CATEGORIES)"; exit 1; }; \
	 if ! ls -d $$p >/dev/null 2>&1; then echo "$$p: nothing there"; exit 0; fi; \
	 size=$$(du -shc $$p | tail -1 | cut -f1); \
	 if [ "$*" = pbf ] && [ "$(FORCE)" != 1 ]; then \
	     echo "$$p holds $$size of downloads that took hours to fetch politely."; \
	     read -p "really delete them? [y/N] " ok; [ "$$ok" = y ] || { echo "kept $$p"; exit 0; }; \
	 fi; \
	 echo "removing $$p ($$size)"; rm -rf $$p

# What a machine that built the map and now only serves it can drop: the
# database is pure scaffolding once the archives exist -- 154 GB standing in
# for 29 GB. `make serve` still works afterwards; the next `make all` or
# `make anon` re-runs load, as it must.
prune: clean-duckdb clean-log
	@echo "kept $(PMTILES)$(if $(wildcard $(ANON)), and $(ANON),) -- still serves: make serve"

# Every byte this repository put on the disk, the checkout itself excepted.
# The downloads go last and ask, because they are the only part that costs
# someone else's bandwidth to get back.
clean: clean-duckdb clean-pmtiles clean-log clean-anon clean-binary clean-pbf

# Move artefacts built before this layout existed into it, rather than have them
# sit at the repo root where nothing will ever clean them up.
adopt: | dirs
	@[ -e minimap.duckdb ] && { [ -e "$(DB)" ] && echo "skip minimap.duckdb -- $(DB) exists" || { echo "minimap.duckdb -> $(DB)"; mv minimap.duckdb "$(DB)"; }; } || true
	@for f in *.osm.pbf data/*.osm.pbf data/countries/*.osm.pbf; do \
	    [ -e "$$f" ] || continue; \
	    n=$$(basename "$$f" | sed 's/-latest//'); \
	    [ -e "$(PBF)/$$n" ] || { echo "$$f -> $(PBF)/$$n"; mv "$$f" "$(PBF)/$$n"; }; \
	done
	@for f in *.log *.log.*; do \
	    [ -e "$$f" ] || continue; \
	    [ -e "$(LOG)/$$f" ] || { echo "$$f -> $(LOG)/$$f"; mv "$$f" "$(LOG)/$$f"; }; \
	done
	@echo 'done -- `make info` should see them now'

# Every file the build produces and what it is for, so that "what is the target"
# has an answer you can read rather than infer from the rules.
targets:
	@echo "deliverable -- copy this to the server, nothing else (this build is NAME=$(NAME))"
	@printf "  %-40s %s\n" "$(PMTILES)/$(NAME).<layer>.pmtiles" "one archive per layer"
	@printf "  %-40s %s\n" "$(ANON)" "the anonymity zones, if 'make anon' ran"
	@echo
	@echo 'scaffolding -- rebuildable, safe to delete, "make clean" removes it' 
	@printf "  %-40s %s\n" "$(DB)" "features + tile_layers + meta"
	@printf "  %-40s %s\n" "$(DUCKDB)/tmp/" "DuckDB spill, 80+ GB at Europe scale"
	@printf "  %-40s %s\n" "$(LOG)/$(NAME).<stage>.log" "one per stage"
	@printf "  %-40s %s\n" "$(DUCKDB)/.load-$(NAME)-<regions>, .bake-…" "which stages are done, for which extracts"
	@printf "  %-40s %s\n" "$(PMTILES)/.export-$(NAME)-<regions>" "same, for the export"
	@echo
	@echo 'inputs -- expensive; only "make clean-pbf" removes them, and it asks first' 
	@printf "  %-40s %s\n" "$(PBF)/<region>.osm.pbf" "the extracts"
	@printf "  %-40s %s\n" "$(LAND)" "coastline; OSM has no ocean"
	@printf "  %-40s %s\n" "$(PBF)/.geofabrik-index.json" "cached region catalogue"

# --- help ------------------------------------------------------------------

help:
	@echo "minimap — OpenStreetMap -> PMTiles"
	@echo
	@echo "  make all         download -> load -> bake -> export  (NAME=$(NAME))"
	@echo "  make download    fetch the extracts named by REGIONS"
	@echo "  make picardie    make all REGIONS=picardie; likewise make france"
	@echo "  make europe      fetch every European extract (31 GB) and build the continent"
	@echo "  make load        PBF -> DuckDB features"
	@echo "  make bake        features -> MVT tiles"
	@echo "  make export      tiles -> PMTiles archive"
	@echo "  make anon        cut k-anonymity zones from the database (see anon/)"
	@echo "  make serve       serve NAME=$(NAME) on :$(PORT) -- with click-for-a-zone if anon ran"
	@echo "  make anon-serve  the zone lookup alone, on :8091"
	@echo "  make test        every test, no data needed (seconds)"
	@echo "  make check       cargo fmt --check and clippy -D warnings"
	@echo "  make build       the binaries, and where they are"
	@echo "  make perf        what the server's caches cost -- see server/tests/"
	@echo "  make working-set how much of the archive has to stay in RAM"
	@echo "  make info        what is in the build right now"
	@echo "  make regions     what Geofabrik publishes"
	@echo "  make disk-stat   what is on the disk: $(DISK_CATEGORIES)"
	@echo "  make clean-<x>   delete one of those categories and nothing else (pbf asks first)"
	@echo "  make prune       delete the scaffolding ($(DUCKDB) $(LOG)), keep the deliverable"
	@echo "  make clean       delete all of them, downloads and compiler output too (asks first)"
	@echo "  make adopt       move pre-existing artefacts into this layout"
	@echo
	@echo "the deliverable"
	@echo "  $(PMTILES)/$(NAME).*.pmtiles   one archive per layer -- this is what ships"
	@echo '  everything else is scaffolding or input -- see: make targets'  
	@echo
	@echo "the map itself -- rungs, classes, thresholds -- is $(TUNING)"
	@echo "  editing it makes load/bake/export stale, and make re-runs them"
	@echo
	@echo "configuration (override on the command line)"
	@printf "  %-9s %-28s %s\n" REGIONS "$(if $(strip $(REGIONS)),$(REGIONS),(every extract in $(PBF)))" "extracts to build from"
	@printf "  %-9s %-28s %s\n" NAME    "$(NAME)"    "what this build is called"
	@printf "  %-9s %-28s %s\n" PBF     "$(PBF)"     "the extracts; only clean-pbf removes them"
	@printf "  %-9s %-28s %s\n" DUCKDB  "$(DUCKDB)"  "the database; scaffolding"
	@printf "  %-9s %-28s %s\n" PMTILES "$(PMTILES)" "the archives; the deliverable"
	@printf "  %-9s %-28s %s\n" MEMORY  "$(if $(MEMORY),$(MEMORY),half of RAM)" "DuckDB budget"
	@printf "  %-9s %-28s %s\n" JOBS    "$(JOBS)"    "concurrent downloads"
	@echo
	@echo "state"
	@for f in $(PBFS) $(LAND); do \
	    if [ -e "$$f" ]; then printf "  [x] %-46s %s\n" "$$f" "$$(du -h $$f | cut -f1)"; \
	    else printf "  [ ] %-46s %s\n" "$$f" "not downloaded"; fi; done
	@for s in "$(LOADED)|load" "$(BAKED)|bake" "$(EXPORTED)|export"; do \
	    f=$${s%%|*}; n=$${s##*|}; \
	    if [ -e "$$f" ]; then printf "  [x] %-46s %s\n" "$$n" "done"; \
	    else printf "  [ ] %-46s %s\n" "$$n" "pending"; fi; done
	@if [ -n "$$(ls -1 $(PMTILES)/$(NAME).*.pmtiles 2>/dev/null)" ]; then printf "  [x] %-46s %s\n" "$(PMTILES)/$(NAME).*.pmtiles" "$$(du -shc $(PMTILES)/$(NAME).*.pmtiles | tail -1 | cut -f1), $$(ls -1 $(PMTILES)/$(NAME).*.pmtiles | wc -l) layers"; \
	 else printf "  [ ] %-46s %s\n" "$(PMTILES)/$(NAME).*.pmtiles" "pending"; fi
	@others="$$(ls -1 $(PMTILES)/*.pmtiles 2>/dev/null | sed 's|.*/||; s|\..*||' | sort -u | grep -vxF '$(NAME)' | tr '\n' ' ' || true)"; \
	 [ -z "$$others" ] || printf "  %-50s %s\n" "" "other builds here: $$others(make serve NAME=...)"
	@if [ -e "$(ANON)" ]; then printf "  [x] %-46s %s\n" "$(ANON)" "$$(du -h $(ANON) | cut -f1)"; \
	 else printf "  [ ] %-46s %s\n" "$(ANON)" "optional -- make anon"; fi
