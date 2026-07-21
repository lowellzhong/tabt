# 用法:
#   make            # release 编译 + 打包 TabT.app + 签名
#   make run        # 打包并启动 TabT.app(第 0 步验收)
#   make cert       # 一次性创建本地稳定签名身份(见下方 CERT_NAME),让 TCC 授权跨重新编译保留
#   make echo       # 跑第 1 步的 PTY 回声环(在当前终端里)
#   make test       # tabt-core 单元测试
#   make bloat      # 体积审计(需要 cargo install cargo-bloat)
#   make clean

APP       := TabT.app
BIN       := target/release/tabt
BUNDLE    := $(APP)/Contents
CERT_NAME := TabT Dev

.PHONY: all run echo test bloat clean cert

all: $(APP)

$(BIN): $(shell find tabt-app tabt-core -name '*.rs') Cargo.toml tabt-app/Cargo.toml tabt-core/Cargo.toml
	cargo build --release --bin tabt

# Signing identity: prefer the stable local "TabT Dev" certificate (see `make cert`) so the
# app's designated requirement doesn't change across rebuilds and macOS TCC folder-access
# grants survive `make run`. Falls back to ad-hoc (re-prompts every rebuild) if `make cert`
# hasn't been run yet.
$(APP): $(BIN) bundle/Info.plist bundle/AppIcon.icns
	rm -rf $(APP)
	mkdir -p $(BUNDLE)/MacOS $(BUNDLE)/Resources
	cp $(BIN) $(BUNDLE)/MacOS/
	cp bundle/Info.plist $(BUNDLE)/
	cp bundle/AppIcon.icns $(BUNDLE)/Resources/
	@if security find-certificate -c "$(CERT_NAME)" >/dev/null 2>&1; then \
		codesign --force --sign "$(CERT_NAME)" $(APP); \
	else \
		echo "==> no '$(CERT_NAME)' signing identity found, falling back to ad-hoc (run 'make cert' once to stop TCC re-prompting on every rebuild)"; \
		codesign --force --sign - $(APP); \
	fi
	@echo "==> built $(APP) ($$(du -sh $(BUNDLE)/MacOS/tabt | cut -f1))"

cert:
	bash bundle/make-dev-cert.sh

run: $(APP)
	-killall tabt 2>/dev/null || true   # 干掉旧实例,否则 open 只会把它前置、不加载新二进制
	open $(APP)

echo:
	cargo run --release --bin pty-echo

test:
	cargo test -p tabt-core

bloat:
	cargo bloat --release --bin tabt -n 20

clean:
	cargo clean
	rm -rf $(APP)
