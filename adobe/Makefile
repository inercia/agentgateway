# Paths relative to this Makefile so `make -C adobe` and `cd adobe && make` both work.
ADOBE_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
REPO_ROOT := $(abspath $(ADOBE_DIR)/..)
# Adobe snapshot/release builds use adobe/Dockerfile (sccache); root Dockerfile matches upstream.
ADOBE_DOCKERFILE := $(ADOBE_DIR)Dockerfile


ADOBE_VERSION ?= $(shell cat $(ADOBE_DIR)version)
PUBLIC_REMOTE ?= https://github.com/agentgateway/agentgateway.git
BUILD_FEATURES ?= agentgateway/ui,agentgateway/adobe
PLATFORM ?= linux/amd64
BUILD_CACHE_DIR ?= $(HOME)/.cache/agentgateway-buildx
# docker-container driver is required for --cache-to; Docker Desktop default is "docker".
BUILDX_BUILDER_NAME ?= agentgateway-builder
# Resolve upstream version from tags. Only consider upstream semver tags
# (`v*`) so that local Adobe release tags (`adobe-*`) do not get picked up
# by `git describe`. If git describe fails to find a tag, fetch tags from
# the public repo and retry exactly once.
UPSTREAM_VERSION ?= $(shell \
	v=$$(git describe --tags --match 'v*' --always --dirty 2>/dev/null); \
	if echo "$$v" | grep -q '^v'; then echo "$$v"; else \
		git fetch $(PUBLIC_REMOTE) --tags >/dev/null 2>&1; \
		git describe --tags --match 'v*' --always --dirty 2>/dev/null; \
	fi)
GIT_REVISION ?= $(shell git rev-parse HEAD)

# Pin VERSION for iterative rebuilds without re-resolving tags: VERSION=dev-local make snapshot
VERSION ?= $(UPSTREAM_VERSION)

SNAPSHOT_REGISTRY = docker-api-platform-snapshot.dr-uw2.adobeitc.com
RELEASE_REGISTRY = docker-api-platform-release.dr-uw2.adobeitc.com
IMAGE_REPO = apiplatform/v2
IMAGE_NAME = agentgateway

ARCH = $(subst linux/,,$(PLATFORM))
SNAPSHOT_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-$(ARCH)
RELEASE_TAG = $(UPSTREAM_VERSION)-adobe-$(ADOBE_VERSION)-$(ARCH)

SNAPSHOT_IMAGE = $(SNAPSHOT_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(SNAPSHOT_TAG)
RELEASE_IMAGE = $(RELEASE_REGISTRY)/$(IMAGE_REPO)/$(IMAGE_NAME):$(RELEASE_TAG)

DOCKER_BUILDX_CACHE = \
	--cache-from type=local,src=$(BUILD_CACHE_DIR) \
	--cache-to type=local,dest=$(BUILD_CACHE_DIR),mode=max

DOCKER_BUILDX = docker buildx build \
	-f $(ADOBE_DOCKERFILE) \
	--builder $(BUILDX_BUILDER_NAME) \
	--platform $(PLATFORM) \
	--build-arg BUILDER=musl \
	--build-arg VERSION=$(VERSION) \
	--build-arg GIT_REVISION=$(GIT_REVISION) \
	--build-arg CARGO_FEATURES=$(BUILD_FEATURES) \
	--build-arg PROFILE=$(PROFILE) \
	$(DOCKER_BUILDX_CACHE) \
	--load \
	--progress=plain

# snapshot/release use buildx local cache; bootstrap builder on first run: make setup-buildx
.PHONY: setup-buildx ensure-buildx-builder check-version

setup-buildx: ensure-buildx-builder

ensure-buildx-builder:
	@if ! docker buildx inspect $(BUILDX_BUILDER_NAME) >/dev/null 2>&1; then \
		echo "Creating buildx builder '$(BUILDX_BUILDER_NAME)' (docker-container driver) for cache export..."; \
		docker buildx create --name $(BUILDX_BUILDER_NAME) --driver docker-container --bootstrap; \
	fi
	@docker buildx use $(BUILDX_BUILDER_NAME) >/dev/null
	@docker buildx inspect --bootstrap $(BUILDX_BUILDER_NAME) >/dev/null

check-version:
	@case "$(UPSTREAM_VERSION)" in \
		v*) ;; \
		*) echo "ERROR: UPSTREAM_VERSION '$(UPSTREAM_VERSION)' does not start with 'v'." >&2; \
		   echo "       Tags could not be resolved. Run 'make fetch-tags' or check network." >&2; \
		   exit 1 ;; \
	esac

.PHONY: snapshot release

# snapshot: quick-release + sccache + buildx cache (default). Prod-like: PROFILE=release make snapshot or make release
snapshot: override PROFILE := $(if $(filter command line environment,$(origin PROFILE)),$(PROFILE),quick-release)
snapshot: check-version ensure-buildx-builder
	$(DOCKER_BUILDX) -t $(SNAPSHOT_IMAGE) $(REPO_ROOT)
	@echo ""
	@echo "Snapshot image: $(SNAPSHOT_IMAGE)"
	@digest=$$(docker image inspect $(SNAPSHOT_IMAGE) --format '{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}' 2>/dev/null); \
		if [ -n "$$digest" ]; then echo "Snapshot digest: $$digest"; fi

release: override PROFILE := $(if $(filter command line environment,$(origin PROFILE)),$(PROFILE),release)
release: check-version ensure-buildx-builder
	$(DOCKER_BUILDX) -t $(RELEASE_IMAGE) $(REPO_ROOT)
	@echo ""
	@echo "Release image: $(RELEASE_IMAGE)"
	@digest=$$(docker image inspect $(RELEASE_IMAGE) --format '{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}' 2>/dev/null); \
		if [ -n "$$digest" ]; then echo "Release digest: $$digest"; fi

.PHONY: fetch-tags
fetch-tags:
	@git fetch $(PUBLIC_REMOTE) --tags

.PHONY: print-snapshot-image print-release-image
print-snapshot-image: check-version
	@echo $(SNAPSHOT_IMAGE)

print-release-image: check-version
	@echo $(RELEASE_IMAGE)
