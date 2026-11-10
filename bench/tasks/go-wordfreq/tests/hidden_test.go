package wordfreq

import (
	"reflect"
	"testing"
)

func TestApostrophesInsideWords(t *testing.T) {
	got := TopWords("don't stop, don't go", 1)
	want := []Count{{"don't", 2}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestMixedScripts(t *testing.T) {
	got := TopWords("Привет world привет WORLD мир 世界 世界 世界", 3)
	want := []Count{{"世界", 3}, {"world", 2}, {"привет", 2}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestNLargerThanTheWordCount(t *testing.T) {
	got := TopWords("b a b", 10)
	want := []Count{{"b", 2}, {"a", 1}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}
