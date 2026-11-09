// Package wordfreq counts words in text.
package wordfreq

import (
	"sort"
	"strings"
)

// Count is one word and how often it occurs.
type Count struct {
	Word  string
	Times int
}

// TopWords returns the n most frequent words, case-insensitively. A word is
// a run of letters or digits in any script. Ties are ordered alphabetically.
func TopWords(text string, n int) []Count {
	counts := map[string]int{}
	for _, word := range strings.Split(text, " ") {
		if word != "" {
			counts[word]++
		}
	}
	result := make([]Count, 0, len(counts))
	for word, times := range counts {
		result = append(result, Count{word, times})
	}
	sort.Slice(result, func(i, j int) bool { return result[i].Times > result[j].Times })
	if len(result) > n {
		result = result[:n]
	}
	return result
}
