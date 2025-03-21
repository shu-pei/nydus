package load

import (
	"context"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/dragonflyoss/image-service/contrib/nydusify/pkg/build"
	"github.com/dragonflyoss/image-service/contrib/nydusify/pkg/converter/provider"
	"github.com/dragonflyoss/image-service/contrib/nydusify/pkg/parser"
	"github.com/sirupsen/logrus"

	"github.com/pkg/errors"
)

type Opt struct {
	Sources        []string
	SourceInsecure bool
	NydusImagePath string
	WorkDir        string
	ExpectedArch   string
}

type Load struct {
	Opt
	sourcesParser []*parser.Parser
}

func New(opt Opt) (*Load, error) {
	// TODO: support sources image resolver
	var sourcesParser []*parser.Parser
	for _, source := range opt.Sources {
		sourcesRemote, err := provider.DefaultRemote(source, opt.SourceInsecure)
		if err != nil {
			return nil, errors.Wrap(err, "Init source image parser")
		}
		sourceParser, err := parser.New(sourcesRemote, opt.ExpectedArch)
		sourcesParser = append(sourcesParser, sourceParser)
		if err != nil {
			return nil, errors.Wrap(err, "Failed to create parser")
		}
	}

	generator := &Load{
		Opt:           opt,
		sourcesParser: sourcesParser,
	}

	return generator, nil
}

// Generate saves multiple Nydus bootstraps into the database one by one.
func (load *Load) Load(ctx context.Context) error {
	var bootstrapPaths []string
	bootstrapPaths, err := load.pull(ctx)

	err = load.load(ctx, bootstrapPaths)
	if err != nil {
		return err
	}
	// return os.RemoveAll(generator.WorkDir)
	return nil
}

func (load *Load) load(_ context.Context, bootstrapSlice []string) error {
	// Invoke "nydus-image" command to granerate
	builder := build.NewBuilder(load.NydusImagePath)

	lifecycleBlobPath := filepath.Join(load.WorkDir, "lifecycle_blob")
	// databaseType := "sqlite"
	// var databasePath string
	// if strings.HasPrefix(generator.WorkDir, "/") {
	// 	databasePath = databaseType + "://" + filepath.Join(generator.WorkDir, "database.db")
	// } else {
	// 	databasePath = databaseType + "://" + filepath.Join(currentDir, generator.WorkDir, "database.db")
	// }
	// outputPath := filepath.Join(generator.WorkDir, "nydus_bootstrap_output.json")

	if err := builder.Load(build.LoadOption{
		BootstrapPaths:    bootstrapSlice,
		LifecycleBlobPath: lifecycleBlobPath,
	}); err != nil {
		return errors.Wrap(err, "invalid nydus bootstrap format")
	}

	logrus.Infof("Successfully Load image chunk lifecycle")
	return nil
}

// Pull the bootstrap of nydus image
func (load *Load) pull(ctx context.Context) ([]string, error) {
	var bootstrapPaths []string
	for index := range load.Sources {
		sourceParsed, err := load.sourcesParser[index].Parse(ctx)
		if err != nil {
			return nil, errors.Wrap(err, "parse Nydus image")
		}

		// Create a directory to store the image bootstrap
		nydusImageName := strings.Replace(load.Sources[index], "/", ":", -1)
		bootstrapDirPath := filepath.Join(load.WorkDir, nydusImageName)
		if err := os.MkdirAll(bootstrapDirPath, fs.ModePerm); err != nil {
			return nil, errors.Wrap(err, "creat work directory")
		}
		if err := load.Output(ctx, sourceParsed, bootstrapDirPath, index); err != nil {
			return nil, errors.Wrap(err, "output image information")
		}
		bootstrapPath := filepath.Join(bootstrapDirPath, "nydus_bootstrap")
		bootstrapPaths = append(bootstrapPaths, bootstrapPath)
	}
	return bootstrapPaths, nil
}
