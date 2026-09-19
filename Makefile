BIN := bin/sph

.PHONY: build test race check clean

build:
	go build -o $(BIN) ./cmd/sph

test:
	go test ./...

race:
	go test -race ./...

check:
	gofmt -l cmd internal
	go vet ./...

clean:
	rm -rf $(BIN)
