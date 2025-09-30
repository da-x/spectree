.PHONY: test build clean

# Build both binaries
build:
	@echo "Building spectree and test-runner..."
	cargo build --bin spectree
	cargo build --bin test-runner

# Run the test
test: build
	@echo "Running tests..."
	./target/debug/test-runner

# Clean build artifacts
clean:
	@echo "Cleaning build artifacts..."
	cargo clean

# Help target
help:
	@echo "Available targets:"
	@echo "  build  - Build both spectree and test-runner binaries"
	@echo "  test   - Build binaries and run tests"
	@echo "  clean  - Clean build artifacts"
	@echo "  help   - Show this help message"