package config

import (
	"strings"
	"testing"
)

func TestDefaultsOnly(t *testing.T) {
	got, err := Load(nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got["port"] != "8080" || got["db.host"] != "127.0.0.1" {
		t.Fatalf("defaults not applied: %v", got)
	}
}

func TestFileOverridesDefaults(t *testing.T) {
	file := strings.NewReader("# settings\nport = 3000\nhost=example.org\n")
	got, err := Load(file, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got["port"] != "3000" || got["host"] != "example.org" || got["debug"] != "false" {
		t.Fatalf("got %v", got)
	}
}

func TestEnvironmentOverridesFile(t *testing.T) {
	file := strings.NewReader("port = 3000\n")
	got, err := Load(file, []string{"PATH=/bin", "APP_PORT=9000"})
	if err != nil {
		t.Fatal(err)
	}
	if got["port"] != "9000" {
		t.Fatalf("port = %q, want 9000", got["port"])
	}
}

func TestBadLine(t *testing.T) {
	if _, err := Load(strings.NewReader("port 3000\n"), nil); err == nil {
		t.Fatal("expected an error for a line without =")
	}
}
