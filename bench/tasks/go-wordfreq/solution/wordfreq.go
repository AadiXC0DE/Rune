// Package wordfreq counts words in text.
package wordfreq

import (
	"sort"
	"strings"
	"unicode"
)

// Count is one word and how often it occurs.
type Count struct {
	Word  string
	Times int
}

// TopWords returns the n most frequent words, case-insensitively. A word is
// a run of letters or digits in any script; an apostrophe between two
// letters or digits stays inside the word. Ties are ordered alphabetically.
func TopWords(text string, n int) []Count {
	counts := map[string]int{}
	runes := []rune(strings.ToLower(text))
	isWord := func(r rune) bool { return unicode.IsLetter(r) || unicode.IsDigit(r) }
	var words []string
	var current []rune
	for i, r := range runes {
		inner := (r == '\'' || r == '\u2019') && i > 0 && i+1 < len(runes) &&
			isWord(runes[i-1]) && isWord(runes[i+1])
		if isWord(r) || inner {
			current = append(current, r)
		} else if len(current) > 0 {
			words = append(words, string(current))
			current = nil
		}
	}
	if len(current) > 0 {
		words = append(words, string(current))
	}
	for _, word := range words {
		counts[word]++
	}
	result := make([]Count, 0, len(counts))
	for word, times := range counts {
		result = append(result, Count{word, times})
	}
	sort.Slice(result, func(i, j int) bool {
		if result[i].Times != result[j].Times {
			return result[i].Times > result[j].Times
		}
		return result[i].Word < result[j].Word
	})
	if n < 0 {
		n = 0
	}
	if len(result) > n {
		result = result[:n]
	}
	return result
}
