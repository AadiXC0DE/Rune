package wordfreq

import (
	"reflect"
	"testing"
)

func TestCountsAcrossPunctuationAndCase(t *testing.T) {
	got := TopWords("The cat. the CAT! a dog, the end", 2)
	want := []Count{{"the", 3}, {"cat", 2}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestTiesAreAlphabetical(t *testing.T) {
	got := TopWords("pear apple fig apple fig pear", 3)
	want := []Count{{"apple", 2}, {"fig", 2}, {"pear", 2}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestUnicodeWordsAndNewlines(t *testing.T) {
	got := TopWords("Café\ncafé naïve\tnaïve naïve 日本", 2)
	want := []Count{{"naïve", 3}, {"café", 2}}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

func TestEmptyAndSmallN(t *testing.T) {
	if got := TopWords("   ", 3); len(got) != 0 {
		t.Fatalf("got %v, want none", got)
	}
	if got := TopWords("a b", 0); len(got) != 0 {
		t.Fatalf("got %v, want none", got)
	}
}
