package config

import "strings"

const envPrefix = "APP_"

// FromEnv picks the settings out of a list of NAME=value strings, as returned
// by os.Environ. APP_PORT becomes "port" and APP_DB_HOST becomes "db.host".
// A variable set to the empty string counts as not set.
func FromEnv(environ []string) map[string]string {
	out := map[string]string{}
	for _, entry := range environ {
		name, value, ok := strings.Cut(entry, "=")
		if !ok || value == "" || !strings.HasPrefix(name, envPrefix) {
			continue
		}
		key := strings.ToLower(strings.TrimPrefix(name, envPrefix))
		out[strings.ReplaceAll(key, "_", ".")] = value
	}
	return out
}
