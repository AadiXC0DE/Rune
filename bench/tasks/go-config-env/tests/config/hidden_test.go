package config

import (
	"strings"
	"testing"
)

func TestHiddenEmptyVariableKeepsFileValue(t *testing.T) {
	got, err := Load(strings.NewReader("port = 3000\n"), []string{"APP_PORT="})
	if err != nil {
		t.Fatal(err)
	}
	if got["port"] != "3000" {
		t.Fatalf("port = %q, want 3000", got["port"])
	}
}

func TestHiddenEmptyVariableKeepsDefault(t *testing.T) {
	got, err := Load(nil, []string{"APP_HOST="})
	if err != nil {
		t.Fatal(err)
	}
	if got["host"] != "localhost" {
		t.Fatalf("host = %q, want localhost", got["host"])
	}
}

func TestHiddenNestedKeys(t *testing.T) {
	file := strings.NewReader("db.host = filehost\ndb.port = 6000\n")
	got, err := Load(file, []string{"APP_DB_HOST=envhost"})
	if err != nil {
		t.Fatal(err)
	}
	if got["db.host"] != "envhost" || got["db.port"] != "6000" {
		t.Fatalf("got %v", got)
	}
}

func TestHiddenUnknownKeysIgnored(t *testing.T) {
	file := strings.NewReader("colour = blue\n")
	got, err := Load(file, []string{"APP_SECRET=x", "OTHER_PORT=1"})
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != len(Defaults()) {
		t.Fatalf("unknown keys leaked: %v", got)
	}
	if got["port"] != "8080" {
		t.Fatalf("port = %q", got["port"])
	}
}

func TestHiddenAllThreeLayers(t *testing.T) {
	file := strings.NewReader("host = filehost\nport = 3000\ndebug = true\n")
	env := []string{"APP_PORT=4000", "APP_DEBUG=", "HOME=/root"}
	got, err := Load(file, env)
	if err != nil {
		t.Fatal(err)
	}
	want := map[string]string{
		"host": "filehost", "port": "4000", "debug": "true",
		"db.host": "127.0.0.1", "db.port": "5432",
	}
	for k, v := range want {
		if got[k] != v {
			t.Errorf("%s = %q, want %q", k, got[k], v)
		}
	}
}

func TestHiddenValueKeepsEquals(t *testing.T) {
	got, err := Load(strings.NewReader("host = a=b\n"), []string{"APP_DB_HOST=x=y"})
	if err != nil {
		t.Fatal(err)
	}
	if got["host"] != "a=b" || got["db.host"] != "x=y" {
		t.Fatalf("got %v", got)
	}
}
