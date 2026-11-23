// Package config resolves settings from defaults, a file and the environment.
package config

import "io"

// Defaults returns the built-in settings. Only these keys are recognised.
func Defaults() map[string]string {
	return map[string]string{
		"host":    "localhost",
		"port":    "8080",
		"debug":   "false",
		"db.host": "127.0.0.1",
		"db.port": "5432",
	}
}

// Load combines the defaults, the settings read from file (which may be nil)
// and the environment. Environment variables win over the file, and the file
// wins over the defaults. Keys outside Defaults are ignored.
func Load(file io.Reader, environ []string) (map[string]string, error) {
	out := Defaults()
	known := Defaults()
	var fromFile map[string]string
	if file != nil {
		parsed, err := ParseFile(file)
		if err != nil {
			return nil, err
		}
		fromFile = parsed
	}
	for key, value := range fromFile {
		if _, ok := known[key]; ok {
			out[key] = value
		}
	}
	for key, value := range FromEnv(environ) {
		if _, ok := known[key]; ok {
			out[key] = value
		}
	}
	return out, nil
}
