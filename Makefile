# minimap — build a vector-tile map from OpenStreetMap.
#
#   make                       what this is, how it is configured, where it got to
#   make all                   download -> load -> bake -> export
#   make all REGIONS=france    ... for a different extract
#   make europe                every European country extract (31 GB, hours)
#   make serve                 serve the archive that came out
#   make clean                 delete everything generated, keep the downloads
#
# One directory per kind of thing, so what a directory holds is its name:
#
#   $(PBF)      what was downloaded. Expensive, polite, identical on every
#               rebuild, so `clean` never touches it.
#   $(DUCKDB)   the database built from it. Enormous, and pure scaffolding once
#               the archives exist.
#   $(PMTILES)  one archive per layer: the deliverable, and all the server needs.
#   $(LOG)      one file per stage.
#
# Each is a variable, which matters because they differ by three orders of
# magnitude. `make all DUCKDB=/mnt/big/duckdb` puts the 154 GB where there is
# room for it without moving the 135 MB off the machine that serves it.

SHELL       := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

# --- configuration ---------------------------------------------------------
# Override any of these on the command line: `make all REGIONS=france`.

# The extracts and the coastline. Never deleted by `clean`; see `distclean`.
PBF     ?= pbf
# The build database and DuckDB's spill. Deleted by `clean`.
DUCKDB  ?= duckdb
# One archive per layer -- what ships. Deleted by `clean`, rebuilt by `export`.
PMTILES ?= pmtiles
# One log per stage. Deleted by `clean`.
LOG     ?= log
# Geofabrik extract names, space separated. `make regions` lists them.
REGIONS ?= picardie
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
# The anonymity-zone index (see anon/README.md), cut from the same features
# table by `make anon`. Optional: `make serve` works without it, and enables
# click-for-a-zone in the viewer when it is there. Deleted by `clean`.
ANON    ?= anon/anon-zones.bin
# Which baked tier the servers answer from. 64 is a city block's worth of
# vagueness (~200 m in Paris, ~1 km in open country); the index also carries 16
# and 256, so changing this is a restart, not a re-bake.
ANON_K  ?= 64
# Extra anon-bake flags: --min-footprint 25 drops the sheds and barns that
# inflate a hamlet's building count, --k 16,64,256 picks the tiers.
ANON_FLAGS ?=

# --- derived ---------------------------------------------------------------

MINIMAP := cargo run --release --quiet --bin minimap --
COMMON  := --pbf $(PBF) --duckdb $(DUCKDB) --pmtiles $(PMTILES) \
           $(if $(MEMORY),--memory $(MEMORY),) --jobs $(JOBS)

LAND  := $(PBF)/land-polygons-split-3857.zip
PBFS  := $(patsubst %,$(PBF)/%.osm.pbf,$(REGIONS))
DB    := $(DUCKDB)/minimap.duckdb

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
# present", so that case hashes the directory listing instead.
REGION_SET := $(if $(strip $(REGIONS)),$(sort $(REGIONS)),$(sort $(notdir $(wildcard $(PBF)/*.osm.pbf))))
REGION_ID  := $(firstword $(shell echo '$(REGION_SET)' | cksum))

LOADED   := $(DUCKDB)/.load-$(REGION_ID)
BAKED    := $(DUCKDB)/.bake-$(REGION_ID)
EXPORTED := $(PMTILES)/.export-$(REGION_ID)

.DEFAULT_GOAL := help
.PHONY: help all download europe load bake export anon serve anon-serve info sql regions clean distclean adopt dirs targets

# --- the pipeline ----------------------------------------------------------
# Each stage writes its own log via --log, rather than being piped through tee.
# A pipe would make the tool's stdout a non-terminal, which is exactly how it
# decides whether to draw a live progress line -- so `| tee` would silently
# trade the thing you are watching for the file you are not.

all: export

download: $(PBFS) $(LAND)

$(PBF)/%.osm.pbf: | dirs
	@$(MINIMAP) download $(COMMON) $*

$(LAND): | dirs
	@$(MINIMAP) download $(COMMON) --land

europe: | dirs
	@$(MINIMAP) download $(COMMON) --log $(LOG)/download.log --europe

load: $(LOADED)
$(LOADED): $(PBFS) $(LAND) $(TUNING) | dirs
	@$(MINIMAP) load $(COMMON) --log $(LOG)/load.log $(REGIONS)
	@touch $@

bake: $(BAKED)
$(BAKED): $(LOADED) $(TUNING) | dirs
	@$(MINIMAP) bake $(COMMON) --log $(LOG)/bake.log
	@touch $@

export: $(EXPORTED)
$(EXPORTED): $(BAKED) | dirs
	@$(MINIMAP) export $(COMMON) --log $(LOG)/export.log
	@touch $@

# The anon index is cut from the same `features` table, so it is stale whenever
# the load is -- and whenever the anon code is, which the pipeline stamps do not
# see. Off the `all` path on purpose: it is a second deliverable, not a stage.
anon: $(ANON)
$(ANON): $(LOADED) $(wildcard anon/format/src/*.rs) $(wildcard anon/bake/src/*.rs) | dirs
	@$(if $(MEMORY),MINIMAP_MEMORY_LIMIT=$(MEMORY) ,)cargo run --release --quiet -p anon-bake -- \
	  --db $(DB) --out $@ $(ANON_FLAGS)

# --- using the result ------------------------------------------------------

serve: $(EXPORTED)
	@MINIMAP_TILES=$(PMTILES) MINIMAP_PORT=$(PORT) ANON_INDEX=$(ANON) ANON_K=$(ANON_K) \
	  cargo run --release --quiet --bin minimap-backend

# The standalone zone service, for deploying the lookup without the map --
# same index, same answers, none of the tiles. See anon/README.md for the
# proxy configuration it needs in front of it (in short: no request logging).
anon-serve: $(ANON)
	@ANON_INDEX=$(ANON) ANON_K=$(ANON_K) cargo run --release --quiet -p anon-serve

info:
	@$(MINIMAP) info $(COMMON)

# make sql Q="select layer, count(*) from features group by 1"
#
# Q travels as an environment variable rather than being pasted into the recipe:
# make would expand a multi-line or quote-bearing query straight into the shell
# command, where its newlines end the command early.
export Q
sql:
	@$(MINIMAP) sql $(COMMON) "$$Q"

regions:
	@$(MINIMAP) regions $(COMMON)

dirs:
	@mkdir -p $(PBF) $(DUCKDB) $(PMTILES) $(LOG)

# --- cleaning --------------------------------------------------------------
# The asymmetry is the point. $(DUCKDB) and $(PMTILES) are a pure function of
# $(PBF) and the settings above, so throwing them away costs only CPU. $(PBF)
# costs someone else's bandwidth, so it takes a second, explicit ask.

clean:
	@for d in $(DUCKDB) $(PMTILES) $(LOG); do \
	    if [ -d "$$d" ]; then echo "removing $$d ($$(du -sh $$d | cut -f1))"; rm -rf "$$d"; fi; \
	done
	@if [ -e "$(ANON)" ]; then echo "removing $(ANON) ($$(du -h $(ANON) | cut -f1))"; rm -f "$(ANON)"; fi
	@echo "kept $(PBF) ($$(du -sh $(PBF) 2>/dev/null | cut -f1 || echo 'nothing yet')) — rebuild with: make all"

distclean: clean
	@if [ -d $(PBF) ]; then \
	    echo; echo "$(PBF) holds $$(du -sh $(PBF) | cut -f1) of downloads that took hours to fetch politely."; \
	    read -p "really delete them? [y/N] " ok; \
	    [ "$$ok" = y ] && rm -rf $(PBF) && echo "removed $(PBF)" || echo "kept $(PBF)"; \
	fi

# Move artefacts built before this layout existed into it, rather than have them
# sit at the repo root where nothing will ever clean them up.
adopt: | dirs
	@[ -e minimap.duckdb ] && { [ -e "$(DB)" ] && echo "skip minimap.duckdb -- $(DB) exists" || { echo "minimap.duckdb -> $(DB)"; mv minimap.duckdb "$(DB)"; }; } || true
	@for f in *.osm.pbf data/*.osm.pbf data/countries/*.osm.pbf; do \
	    [ -e "$$f" ] || continue; \
	    n=$$(basename "$$f" | sed 's/-latest//'); \
	    [ -e "$(PBF)/$$n" ] || { echo "$$f -> $(PBF)/$$n"; mv "$$f" "$(PBF)/$$n"; }; \
	done
	@echo 'done -- `make info` should see them now'

# Every file the build produces and what it is for, so that "what is the target"
# has an answer you can read rather than infer from the rules.
targets:
	@echo "deliverable -- copy this to the server, nothing else"
	@printf "  %-40s %s\n" "$(PMTILES)/<layer>.pmtiles" "one archive per layer"
	@printf "  %-40s %s\n" "$(ANON)" "the anonymity zones, if 'make anon' ran"
	@echo
	@echo 'scaffolding -- rebuildable, safe to delete, "make clean" removes it' 
	@printf "  %-40s %s\n" "$(DB)" "features + tile_layers + meta"
	@printf "  %-40s %s\n" "$(DUCKDB)/tmp/" "DuckDB spill, 80+ GB at Europe scale"
	@printf "  %-40s %s\n" "$(LOG)/<stage>.log" "one per stage"
	@printf "  %-40s %s\n" "$(DUCKDB)/.load-<regions>, .bake-…" "which stages are done, for which extracts"
	@echo
	@echo 'inputs -- expensive; "make clean" keeps them, "make distclean" removes them' 
	@printf "  %-40s %s\n" "$(PBF)/<region>.osm.pbf" "the extracts"
	@printf "  %-40s %s\n" "$(LAND)" "coastline; OSM has no ocean"
	@printf "  %-40s %s\n" "$(PBF)/.geofabrik-index.json" "cached region catalogue"
	@echo
	@printf "  %-40s %s\n" "$(EXPORTED)" "and export"

# --- help ------------------------------------------------------------------

help:
	@echo "minimap — OpenStreetMap -> PMTiles"
	@echo
	@echo "  make all         download -> load -> bake -> export"
	@echo "  make download    fetch the extracts named by REGIONS"
	@echo "  make europe      fetch every European country extract (31 GB)"
	@echo "  make load        PBF -> DuckDB features"
	@echo "  make bake        features -> MVT tiles"
	@echo "  make export      tiles -> PMTiles archive"
	@echo "  make anon        cut k-anonymity zones from the database (see anon/)"
	@echo "  make serve       serve it on :$(PORT) -- with click-for-a-zone if anon ran"
	@echo "  make anon-serve  the zone lookup alone, on :8091"
	@echo "  make info        what is in the build right now"
	@echo "  make regions     what Geofabrik publishes"
	@echo "  make clean       delete $(DUCKDB) $(PMTILES) $(LOG), keep $(PBF)"
	@echo "  make distclean   delete the downloads too (asks first)"
	@echo "  make adopt       move pre-existing artefacts into this layout"
	@echo
	@echo "the deliverable"
	@echo "  $(PMTILES)/*.pmtiles   one archive per layer -- this is what ships"
	@echo '  everything else is scaffolding or input -- see: make targets'  
	@echo
	@echo "the map itself -- rungs, classes, thresholds -- is $(TUNING)"
	@echo "  editing it makes load/bake/export stale, and make re-runs them"
	@echo
	@echo "configuration (override on the command line)"
	@printf "  %-9s %-28s %s\n" REGIONS "$(if $(strip $(REGIONS)),$(REGIONS),(every extract in $(PBF)))" "extracts to build from"
	@printf "  %-9s %-28s %s\n" PBF     "$(PBF)"     "the extracts, never cleaned"
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
	@if [ -n "$$(ls -1 $(PMTILES)/*.pmtiles 2>/dev/null)" ]; then printf "  [x] %-46s %s\n" "$(PMTILES)/" "$$(du -sh $(PMTILES) | cut -f1), $$(ls -1 $(PMTILES)/*.pmtiles | wc -l) layers"; \
	 else printf "  [ ] %-46s %s\n" "$(PMTILES)/" "pending"; fi
	@if [ -e "$(ANON)" ]; then printf "  [x] %-46s %s\n" "$(ANON)" "$$(du -h $(ANON) | cut -f1)"; \
	 else printf "  [ ] %-46s %s\n" "$(ANON)" "optional -- make anon"; fi
