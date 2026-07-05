# 用法:
#   make            # release 编译 + 打包 TabT.app + ad-hoc 签名
#   make run        # 打包并启动 TabT.app(第 0 步验收)
#   make echo       # 跑第 1 步的 PTY 回声环(在当前终端里)
#   make test       # tabt-core 单元测试
#   make bloat      # 体积审计(需要 cargo install cargo-bloat)
#   make clean

APP      := TabT.app
BIN      := target/release/tabt
BUNDLE   := $(APP)/Contents

.PHONY: all run echo test bloat clean

all: $(APP)

$(BIN): $(shell find tabt-app tabt-core -name '*.rs') Cargo.toml tabt-app/Cargo.toml tabt-core/Cargo.toml
	cargo build --release --bin tabt

$(APP): $(BIN) bundle/Info.plist bundle/AppIcon.icns
	rm -rf $(APP)
	mkdir -p $(BUNDLE)/MacOS $(BUNDLE)/Resources
	cp $(BIN) $(BUNDLE)/MacOS/
	cp bundle/Info.plist $(BUNDLE)/
	cp bundle/AppIcon.icns $(BUNDLE)/Resources/
	codesign --force --sign - $(APP)
	@echo "==> built $(APP) ($$(du -sh $(BUNDLE)/MacOS/tabt | cut -f1))"

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
