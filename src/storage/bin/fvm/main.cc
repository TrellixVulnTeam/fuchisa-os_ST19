// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <unistd.h>

#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <limits>
#include <memory>
#include <optional>
#include <string>

#include <fbl/alloc_checker.h>
#include <fbl/auto_call.h>
#include <safemath/checked_math.h>

#include "range/interval-tree.h"
#include "src/storage/bin/fvm/mtd.h"
#include "src/storage/blobfs/format.h"
#include "src/storage/fvm/host/container.h"
#include "src/storage/fvm/host/file_wrapper.h"
#include "src/storage/fvm/host/format.h"
#include "src/storage/fvm/host/fvm_container.h"
#include "src/storage/fvm/host/fvm_reservation.h"
#include "src/storage/fvm/host/sparse_container.h"
#include "src/storage/fvm/sparse_reader.h"
#include "src/storage/volume_image/ftl/ftl_image.h"
#include "src/storage/volume_image/ftl/ftl_raw_nand_image_writer.h"
#include "src/storage/volume_image/ftl/options.h"
#include "src/storage/volume_image/ftl/raw_nand_image.h"
#include "src/storage/volume_image/ftl/raw_nand_image_utils.h"
#include "src/storage/volume_image/fvm/fvm_sparse_image.h"
#include "src/storage/volume_image/fvm/fvm_sparse_image_reader.h"
#include "src/storage/volume_image/options.h"
#include "src/storage/volume_image/utils/decompressor.h"
#include "src/storage/volume_image/utils/fd_reader.h"
#include "src/storage/volume_image/utils/fd_writer.h"

#define DEFAULT_SLICE_SIZE (8lu * (1 << 20))
constexpr char kMinimumInodes[] = "--minimum-inodes";
constexpr char kMinimumData[] = "--minimum-data-bytes";
constexpr char kMaximumBytes[] = "--maximum-bytes";
constexpr char kEmptyMinfs[] = "--with-empty-minfs";
constexpr char kReserveSlices[] = "--reserve-slices";

enum class DiskType {
  File = 0,
  Mtd = 1,
};

int usage(void) {
  fprintf(stderr, "usage: fvm [ output_path ] [ command ] [ <flags>* ] [ <input_paths>* ]\n");
  fprintf(stderr, "fvm performs host-side FVM and sparse file creation\n");
  fprintf(stderr, "Commands:\n");
  fprintf(stderr, " create : Creates an FVM partition\n");
  fprintf(stderr,
          " add : Adds a Minfs or Blobfs partition to an FVM (input path is"
          " required)\n");
  fprintf(stderr,
          " extend : Extends an FVM container to the specified size (length is"
          " required)\n");
  fprintf(stderr,
          " ftl-raw-nand: converts the input fvm.sparse.blk into a FTL Raw Nand Image (--sparse is "
          "required).\n");
  fprintf(stderr, " sparse : Creates a sparse file. One or more input paths are required.\n");
  fprintf(stderr, " pave : Creates an FVM container from a sparse file.\n");
  fprintf(stderr,
          " verify : Report basic information about sparse/fvm files and run fsck on"
          " contained partitions.\n");
  fprintf(stderr,
          " check : verifies that the |--sparse| image provided is valid. if |--max_disk_size| is "
          "provided check that the maximum disk size is set to such value in the sparse image.\n");
  fprintf(stderr,
          " size : Prints the minimum size required in order to pave a sparse file."
          " If the --disk flag is provided, instead checks that the paved sparse file"
          " will fit within a disk of this size. On success, no information is"
          " outputted\n");
  fprintf(stderr,
          " used-data-size : Prints sum of the space, in bytes, used by data on \n"
          " different partitions. This does not include blocks used internally for \n"
          " superblock, bitmaps, inodes, or for journal,\n");
  fprintf(stderr, " used-inodes : Prints the sum of used inodes on different partitions.\n");
  fprintf(stderr,
          " used-size : Prints sum of the space, in bytes, used by data and by\n"
          " superblock, bitmaps, inodes, and journal different partitions. All of the\n"
          " reservations for non-data blocks are considered as used.\n");
  fprintf(stderr,
          " decompress : Decompresses a compressed sparse/raw file. --sparse/lz4/default input "
          "path is required. If option is set to --default, the tool will attempt to detect the "
          "input format\n");
  fprintf(stderr, "Flags (neither or both of offset/length must be specified):\n");
  fprintf(stderr,
          " --slice [bytes] - specify slice size - only valid on container creation.\n"
          "                   (default: %zu)\n",
          DEFAULT_SLICE_SIZE);
  fprintf(stderr,
          " --max-disk-size [bytes] Used for preallocating metadata. Only valid for sparse image. "
          "(defaults to 0)\n");
  fprintf(stderr, " --offset [bytes] - offset at which container begins (fvm only)\n");
  fprintf(stderr, " --length [bytes] - length of container within file (fvm only)\n");
  fprintf(stderr,
          " --compress - specify that file should be compressed (sparse and android sparse image "
          "only)\n");
  fprintf(stderr, " --disk [bytes] - Size of target disk (valid for size command only)\n");
  fprintf(stderr, " --disk-type [file OR mtd] - Type of target disk (pave only)\n");
  fprintf(stderr, " --max-bad-blocks [number] - Max bad blocks for FTL (pave on mtd only)\n");
  fprintf(stderr, "Input options:\n");
  fprintf(stderr, " --blob [path] [reserve options] - Add path as blob type (must be blobfs)\n");
  fprintf(stderr,
          " --data [path] [reserve options] - Add path as encrypted data type (must"
          " be minfs)\n");
  fprintf(stderr, " --data-unsafe [path] - Add path as unencrypted data type (must be minfs)\n");
  fprintf(stderr, " --system [path] - Add path as system type (must be minfs)\n");
  fprintf(stderr, " --default [path] - Add generic path\n");
  fprintf(stderr, " --sparse [path] - Path to compressed sparse file\n");
  fprintf(stderr, " --lz4 [path] - Path to lz4 compressed raw file\n");
  fprintf(stderr, " --raw [path] - Path to raw fvm image file\n");
  fprintf(stderr,
          " --resize-image-file-to-fit - When used with create/extend command, the output image "
          "file will "
          "be resized to just fit the metadata header and added partitions. Disk size specified in "
          "the header remains the same. It's useful for reducing the size of the image file for "
          "flashing\n");
  fprintf(stderr,
          " --android-sparse-format - When used with create command, the image will be converted "
          "to android sparse image.\n");
  fprintf(
      stderr,
      " --length-is-lowerbound - When used with extend command, if current disk size is already "
      "no smaller than the specified size, the command will be no-op. If the option is not "
      "specified, it will error out in this case.\n");
  fprintf(stderr, "reserve options:\n");
  fprintf(stderr,
          " These options, on success, reserve additional fvm slices for data/inodes.\n"
          " The number of bytes reserved may exceed the actual bytes needed due to\n"
          " rounding up to slice boundary.\n");
  fprintf(stderr,
          " --minimum-inodes inode_count - number of inodes to reserve\n"
          "                                Blobfs inode size is %u\n"
          "                                Minfs inode size is %u\n",
          blobfs::kBlobfsInodeSize, minfs::kMinfsInodeSize);

  fprintf(stderr,
          " --minimum-data-bytes data_bytes - number of bytes to reserve for data\n"
          "                                   in the fs\n"
          "                                   Blobfs block size is %u\n"
          "                                   Minfs block size is %u\n",
          blobfs::kBlobfsBlockSize, minfs::kMinfsBlockSize);
  fprintf(stderr,
          " --maximum-bytes bytes - Places an upper bound of <bytes> on the total\n"
          "                         number of bytes which may be used by the partition.\n"
          "                         Returns an error if more space is necessary to\n"
          "                         create the requested filesystem.\n");
  fprintf(stderr,
          " --with-empty-minfs    - Adds a placeholder partition that will be formatted on boot,\n"
          "                         to minfs. The partition will be the 'data' partition.\n");
  fprintf(
      stderr,
      "   --nand-page-size : Sets the hardware page size in bytes used by the targetted device.\n");
  fprintf(stderr,
          "   --nand-oob-size : Sets the hardware page oob size in bytes used by the targetted "
          "device.\n");
  fprintf(stderr,
          "   --nand-pages-per-block : Sets the number of pages per block in the device.\n");
  fprintf(stderr, "   --nand-block-count : Sets the number of blocks in the device.\n");
  exit(-1);
}

int parse_size(const char* size_str, size_t* out) {
  char* end;
  size_t size = strtoull(size_str, &end, 10);

  switch (end[0]) {
    case 'K':
    case 'k':
      size *= 1024;
      end++;
      break;
    case 'M':
    case 'm':
      size *= (1024 * 1024);
      end++;
      break;
    case 'G':
    case 'g':
      size *= (1024 * 1024 * 1024);
      end++;
      break;
  }

  if (end[0] || size == 0) {
    fprintf(stderr, "Bad size: %s\n", size_str);
    return -1;
  }

  *out = size;
  return 0;
}

class RawBlockImageWriter final : public storage::volume_image::Writer {
 public:
  explicit RawBlockImageWriter(storage::volume_image::Writer* writer) : writer_(writer) {}

  fit::result<void, std::string> Write(uint64_t offset, fbl::Span<const uint8_t> buffer) final {
    ranges_.insert(range::Range<>(offset, offset + buffer.size()));
    return writer_->Write(offset, buffer);
  }

  fit::result<void, std::string> VisitGaps(
      fit::function<fit::result<void, std::string>(uint64_t start, uint64_t end, Writer* writer)>
          visitor) {
    uint64_t last_gap_end = 0;
    for (const auto& range : ranges_) {
      if (range.second.Start() > last_gap_end) {
        auto visit_result = visitor(last_gap_end, range.second.Start(), writer_);
        if (visit_result.is_error()) {
          return visit_result.take_error_result();
        }
      }
      last_gap_end = range.second.End();
    }
    return fit::ok();
  }

 private:
  // Keep track of written ranges.
  range::IntervalTree<range::Range<>> ranges_;

  storage::volume_image::Writer* writer_ = nullptr;
};

int add_partitions(Container* container, int argc, char** argv) {
  // If the flag |kEmptyMinfs| is set, an empty minfs partition will be added after the rest of the
  // partitions have been processed.
  bool add_empty_minfs = false;
  // If the flag |kReserveSlices| is set, a reservation partition with the desired number of slices
  // will be added after the rest of the partitions have been processed.
  size_t slices_to_reserve = 0;

  for (int i = 0; i < argc;) {
    if (argc - i < 2 || argv[i][0] != '-' || argv[i][1] != '-') {
      usage();
    }

    const char* partition_type = argv[i] + 2;
    const char* partition_path = argv[i + 1];
    if (i < argc && strcmp(argv[i], kEmptyMinfs) == 0) {
      add_empty_minfs = true;
      i++;
      continue;
    }
    if (i < argc && strcmp(argv[i], kReserveSlices) == 0) {
      if (parse_size(argv[i + 1], &slices_to_reserve) < 0) {
        usage();
        return -1;
      }
      i += 2;
      continue;
    }

    std::optional<uint64_t> inodes = {}, data = {}, total_bytes = {};
    i += 2;

    while (true) {
      size_t size;
      if ((i + 2) <= argc && strcmp(argv[i], kMinimumInodes) == 0) {
        if (parse_size(argv[i + 1], &size) < 0) {
          usage();
          return -1;
        }
        inodes = safemath::checked_cast<uint64_t>(size);
        i += 2;
      } else if ((i + 2) <= argc && strcmp(argv[i], kMinimumData) == 0) {
        if (parse_size(argv[i + 1], &size) < 0) {
          usage();
          return -1;
        }
        data = safemath::checked_cast<uint64_t>(size);
        i += 2;
      } else if ((i + 2) <= argc && strcmp(argv[i], kMaximumBytes) == 0) {
        if (parse_size(argv[i + 1], &size) < 0) {
          usage();
          return -1;
        }
        total_bytes = safemath::checked_cast<uint64_t>(size);
        i += 2;
      } else {
        break;
      }
    }

    FvmReservation reserve(inodes, data, total_bytes);
    zx_status_t status = container->AddPartition(partition_path, partition_type, &reserve);
    if (status != ZX_OK) {
      fprintf(stderr, "Failed to add partition: %d\n", status);
      reserve.Dump(stderr);
      return -1;
    }
  }
  if (add_empty_minfs) {
    container->AddCorruptedPartition(kDataTypeName, 0);
  }
  if (slices_to_reserve) {
    container->AddSnapshotMetadataPartition(slices_to_reserve);
  }

  return 0;
}

size_t get_disk_size(const char* path, size_t offset) {
  fbl::unique_fd fd(open(path, O_RDONLY, 0644));

  if (fd) {
    struct stat s;
    if (fstat(fd.get(), &s) < 0) {
      fprintf(stderr, "Failed to stat %s\n", path);
      exit(-1);
    }

    return s.st_size - offset;
  }

  return 0;
}

zx_status_t ParseDiskType(const char* type_str, DiskType* out) {
  if (!strcmp(type_str, "file")) {
    *out = DiskType::File;
    return ZX_OK;
  } else if (!strcmp(type_str, "mtd")) {
    *out = DiskType::Mtd;
    return ZX_OK;
  }

  fprintf(stderr, "Unknown disk type: '%s'. Expected 'file' or 'mtd'.\n", type_str);
  return ZX_ERR_INVALID_ARGS;
}

bool IsRawFvmImageFile(const char* path, size_t offset) {
  zx_status_t status = FvmContainer::Verify(path, offset);
  return status == ZX_OK;
}

zx_status_t IsFvmSparseImageFile(const char* path, bool* result) {
  fbl::unique_fd fd(open(path, O_RDONLY, 0644));
  if (!fd) {
    fprintf(stderr, "Fail to open file %s\n", path);
    return ZX_ERR_IO;
  }
  std::unique_ptr<fvm::SparseReader> reader;
  *result = fvm::SparseReader::CreateSilent(std::move(fd), &reader) == ZX_OK;
  return ZX_OK;
}

zx_status_t IsLZ4CompressedFile(const char* path, bool* result) {
  constexpr uint32_t kLZ4Magic = 0x184D2204;
  fbl::unique_fd fd(open(path, O_RDONLY, 0644));
  if (!fd) {
    fprintf(stderr, "Fail to open file %s\n", path);
    return ZX_ERR_IO;
  }
  uint32_t magic;
  if (read(fd.get(), &magic, sizeof(magic)) != sizeof(magic)) {
    fprintf(stderr, "Fail to read from file %s\n", path);
    return ZX_ERR_IO;
  }
  *result = magic == kLZ4Magic;
  return ZX_OK;
}

const char* DetermineImageInputTypeOption(const char* input_path, size_t offset) {
  bool result;
  if (IsRawFvmImageFile(input_path, offset)) {
    return "--raw";
  }

  if (IsFvmSparseImageFile(input_path, &result) != ZX_OK) {
    return nullptr;
  }
  if (result) {
    return "--sparse";
  }

  if (IsLZ4CompressedFile(input_path, &result) != ZX_OK) {
    return nullptr;
  }
  if (result) {
    return "--lz4";
  }

  return nullptr;
}

zx_status_t CopyFile(const char* dst, const char* src) {
  constexpr size_t kBufferLength = 1024 * 1024;
  fbl::unique_fd fd_src(open(src, O_RDONLY, 0644));
  if (!fd_src) {
    fprintf(stderr, "Unable to open source file %s\n", src);
    return ZX_ERR_IO;
  }

  fbl::unique_fd fd_dst(open(dst, O_RDWR | O_CREAT, 0644));
  if (!fd_dst) {
    fprintf(stderr, "Unable to create output file %s\n", dst);
    return ZX_ERR_IO;
  }

  std::vector<uint8_t> buffer(kBufferLength);
  while (true) {
    ssize_t read_bytes = read(fd_src.get(), buffer.data(), kBufferLength);
    if (read_bytes < 0) {
      fprintf(stderr, "Failed to read data from image file\n");
      return ZX_ERR_IO;
    } else if (read_bytes == 0) {
      break;
    }

    if (write(fd_dst.get(), buffer.data(), read_bytes) != read_bytes) {
      fprintf(stderr, "BlockReader: failed to write to output\n");
      return ZX_ERR_IO;
    }
  }
  return ZX_OK;
}

int main(int argc, char** argv) {
  if (argc < 3) {
    usage();
  }

  int i = 1;
  const char* path = argv[i++];     // Output path
  const char* command = argv[i++];  // Command
  if (strcmp(path, "check") == 0) {
    command = path;
    i--;
  }

  size_t length = 0;
  size_t offset = 0;
  size_t slice_size = DEFAULT_SLICE_SIZE;
  size_t disk_size = 0;

  size_t max_bad_blocks = 0;
  size_t max_disk_size = 0;
  bool is_max_bad_blocks_set = false;
  DiskType disk_type = DiskType::File;

  bool should_unlink = true;
  bool resize_image_file_to_fit = false;
  bool length_is_lower_bound = false;
  bool convert_to_android_sparse_format = false;
  uint32_t flags = 0;
  size_t block_count = 0;
  storage::volume_image::RawNandOptions options;

  while (i < argc) {
    if (!strcmp(argv[i], "--slice") && i + 1 < argc) {
      if (parse_size(argv[++i], &slice_size) < 0) {
        return -1;
      }
      if (!slice_size || slice_size % blobfs::kBlobfsBlockSize ||
          slice_size % minfs::kMinfsBlockSize) {
        fprintf(stderr, "Invalid slice size - must be a multiple of %u and %u\n",
                blobfs::kBlobfsBlockSize, minfs::kMinfsBlockSize);
        return -1;
      }
    } else if (!strcmp(argv[i], "--offset") && i + 1 < argc) {
      should_unlink = false;
      if (parse_size(argv[++i], &offset) < 0) {
        return -1;
      }
    } else if (!strcmp(argv[i], "--length") && i + 1 < argc) {
      if (parse_size(argv[++i], &length) < 0) {
        return -1;
      }
    } else if (!strcmp(argv[i], "--compress")) {
      if (!strcmp(argv[++i], "lz4")) {
        flags |= fvm::kSparseFlagLz4;
      } else {
        fprintf(stderr, "Invalid compression type\n");
        return -1;
      }
    } else if (!strcmp(argv[i], "--disk-type")) {
      if (ParseDiskType(argv[++i], &disk_type) != ZX_OK) {
        return -1;
      }
    } else if (!strcmp(argv[i], "--max-bad-blocks")) {
      max_bad_blocks = std::stoul(argv[++i], nullptr, 10);
      is_max_bad_blocks_set = true;
    } else if (!strcmp(argv[i], "--disk")) {
      if (parse_size(argv[++i], &disk_size) < 0) {
        return -1;
      }
    } else if (!strcmp(argv[i], "--max-disk-size") && i + 1 < argc) {
      if (parse_size(argv[++i], &max_disk_size) < 0) {
        return -1;
      }
    } else if (!strcmp(argv[i], "--resize-image-file-to-fit")) {
      resize_image_file_to_fit = true;
    } else if (!strcmp(argv[i], "--length-is-lowerbound")) {
      length_is_lower_bound = true;
    } else if (!strcmp(argv[i], "--android-sparse-format")) {
      convert_to_android_sparse_format = true;
    } else if (!strcmp(argv[i], "--nand-page-size")) {
      size_t page_size = 0;
      if (parse_size(argv[++i], &page_size) < 0) {
        return -1;
      }
      options.page_size = static_cast<uint64_t>(page_size);
    } else if (!strcmp(argv[i], "--nand-oob-size")) {
      size_t oob_bytes_size = 0;
      if (parse_size(argv[++i], &oob_bytes_size) < 0) {
        return -1;
      }
      if (oob_bytes_size > std::numeric_limits<uint8_t>::max()) {
        fprintf(stderr, "OOB Byte size must lower than 256 bytes.\n");
        return -1;
      }
      options.oob_bytes_size = static_cast<uint8_t>(oob_bytes_size);
    } else if (!strcmp(argv[i], "--nand-pages-per-block")) {
      size_t pages_per_block = 0;
      if (parse_size(argv[++i], &pages_per_block) < 0) {
        return -1;
      }
      if (pages_per_block > std::numeric_limits<uint32_t>::max()) {
        fprintf(stderr, "Pages Per Block must be lower than 4,294,967,296.\n");
        return -1;
      }
      options.pages_per_block = static_cast<uint32_t>(pages_per_block);
    } else if (!strcmp(argv[i], "--nand-block-count")) {
      if (parse_size(argv[++i], &block_count) < 0) {
        return -1;
      }
    } else {
      break;
    }

    ++i;
  }

  if (!strcmp(command, "create") && should_unlink) {
    unlink(path);
  }

  if (strcmp(command, "check") == 0) {
    char* input_type = argv[i];
    char* input_path = argv[i + 1];
    if (strcmp(input_type, "--sparse") != 0) {
      usage();
      return -1;
    }
    auto sparse_image_reader_or = storage::volume_image::FdReader::Create(input_path);
    if (sparse_image_reader_or.is_error()) {
      fprintf(stderr, "%s\n", sparse_image_reader_or.error().c_str());
      return -1;
    }

    auto header_or =
        storage::volume_image::FvmSparseImageGetHeader(0, sparse_image_reader_or.value());
    if (header_or.is_error()) {
      fprintf(stderr, "Failed to parse sparse image header. %s\n", header_or.error().c_str());
      return -1;
    }
    auto header = header_or.take_value();

    if (max_disk_size != 0 && header.maximum_disk_size != max_disk_size) {
      fprintf(stderr, "Sparse image does not match max disk size. Found %lu, expected %lu.\n",
              static_cast<size_t>(header.maximum_disk_size), max_disk_size);
      return -1;
    }

    auto partitions_or = storage::volume_image::FvmSparseImageGetPartitions(
        sizeof(header), sparse_image_reader_or.value(), header);
    if (partitions_or.is_error()) {
      fprintf(stderr, "Failed to parse sparse image partition metadata. %s\n",
              partitions_or.error().c_str());
      return -1;
    }
    auto partitions = partitions_or.take_value();

    int expected_data_length = 0;
    int total_size = sparse_image_reader_or.value().GetMaximumOffset();
    for (const auto& partition : partitions) {
      for (const auto& extent : partition.extents) {
        expected_data_length += extent.extent_length;
      }
    }

    auto compression_options = storage::volume_image::FvmSparseImageGetCompressionOptions(header);
    // Decompress the image.
    if (compression_options.schema != storage::volume_image::CompressionSchema::kNone) {
      std::unique_ptr<SparseContainer> compressedContainer;
      if (SparseContainer::CreateExisting(input_path, &compressedContainer) != ZX_OK) {
        return -1;
      }
      std::string tmp_path = std::filesystem::temp_directory_path().generic_string() +
                             "/decompressed_sparse_fvm_XXXXXX";
      fbl::unique_fd created_file(mkstemp(tmp_path.data()));

      if (!created_file.is_valid()) {
        fprintf(stderr, "Failed to create temporary file for decompressing image. %s\n",
                strerror(errno));
        return -1;
      }

      if (compressedContainer->Decompress(tmp_path.c_str()) != ZX_OK) {
        return -1;
      }

      auto reader_or = storage::volume_image::FdReader::Create(tmp_path);
      if (reader_or.is_error()) {
        fprintf(stderr, "%s\n", reader_or.error().c_str());
        return -1;
      }
      total_size = reader_or.value().GetMaximumOffset() - header.header_length;
    }

    if (expected_data_length > total_size) {
      fprintf(stderr, "Extent accumulated length is %d, uncompressed data is %d\n",
              expected_data_length, total_size);
      return -1;
    }

    fprintf(stderr, "--sparse input file is a valid FVM Sparse Image.\n");
    return 0;
  }

  if (!strcmp(command, "ftl-raw-nand")) {
    char* input_type = argv[i];
    char* input_path = argv[i + 1];

    if (strcmp(input_type, "--sparse") != 0) {
      usage();
      return -1;
    }

    if (options.page_size == 0) {
      fprintf(stderr, "Raw Nand device page size must be greater than zero.\n");
      return -1;
    }

    if (options.oob_bytes_size == 0) {
      fprintf(stderr, "Raw Nand device page oob size must be greater than zero.\n");
      return -1;
    }

    if (options.pages_per_block == 0) {
      fprintf(stderr, "Raw Nand device pages per block must be greater than zero.\n");
      return -1;
    }

    if (block_count == 0) {
      fprintf(stderr, "Raw Nand device block count must be greater than zero.\n");
      return -1;
    }

    options.page_count = block_count * options.pages_per_block;
    if (i >= argc) {
      fprintf(stderr, "Missing input path for fvm sparse image.\n");
      return -1;
    }

    auto sparse_image_reader_or = storage::volume_image::FdReader::Create(input_path);
    if (sparse_image_reader_or.is_error()) {
      fprintf(stderr, "%s\n", sparse_image_reader_or.error().c_str());
      return -1;
    }

    // The FTL writer intentionally leaves existing content in place when
    // opening a file, so we need to delete the output file first so that any
    // existing file data doesn't carry over - in particular, if the existing
    // NAND image is larger than the one we're about to generate, the excess
    // data would be left in-place, corrupting the FTL metadata.
    {
      fbl::unique_fd ftl_output(open(path, O_CREAT | O_RDWR, 0644));
      if (!ftl_output.is_valid()) {
        fprintf(stderr, "Failed to create output path. Error %s.\n", strerror(errno));
        return -1;
      }
      if (ftruncate(ftl_output.get(), 0) != 0) {
        fprintf(stderr, "Failed to truncate output path. Error %s.\n", strerror(errno));
        return -1;
      }
    }

    auto sparse_image_reader = sparse_image_reader_or.take_value();
    auto ftl_image_writer_or = storage::volume_image::FdWriter::Create(path);
    if (ftl_image_writer_or.is_error()) {
      fprintf(stderr, "%s\n", ftl_image_writer_or.error().c_str());
      return -1;
    }
    auto ftl_image_writer = ftl_image_writer_or.take_value();
    RawBlockImageWriter raw_writer(&ftl_image_writer);

    std::optional<uint64_t> max_disk_size_opt = std::nullopt;
    if (max_disk_size != 0) {
      max_disk_size_opt = max_disk_size;
    }

    auto fvm_partition_or =
        storage::volume_image::OpenSparseImage(sparse_image_reader, max_disk_size_opt);
    if (fvm_partition_or.is_error()) {
      fprintf(stderr, "%s\n", fvm_partition_or.error().c_str());
      return -1;
    }

    std::vector<storage::volume_image::RawNandImageFlag> flags = {
        storage::volume_image::RawNandImageFlag::kRequireWipeBeforeFlash};
    auto raw_nand_image_writer_or = storage::volume_image::FtlRawNandImageWriter::Create(
        options, flags, storage::volume_image::ImageFormat::kRawImage, &raw_writer);
    if (raw_nand_image_writer_or.is_error()) {
      fprintf(stderr, "%s\n", raw_nand_image_writer_or.error().c_str());
      return -1;
    }

    auto [raw_nand_image_writer, ftl_options] = raw_nand_image_writer_or.take_value();
    auto ftl_image_write_result = storage::volume_image::FtlImageWrite(
        ftl_options, fvm_partition_or.value(), &raw_nand_image_writer);
    if (ftl_image_write_result.is_error()) {
      fprintf(stderr, "%s\n", ftl_image_write_result.error().c_str());
      return -1;
    }

    // Write in the gaps in the image with 0xFF or 'unwritten' bytes.
    // For a raw image there may not be any gaps.
    std::vector<uint8_t> filler(4 << 10, 0xFF);
    auto fill_result = raw_writer.VisitGaps(
        [&filler](uint64_t start, uint64_t end,
                  storage::volume_image::Writer* writer) -> fit::result<void, std::string> {
          uint64_t length = end - start;
          if (filler.size() < length) {
            filler.resize(length, 0xFF);
          }
          return writer->Write(start, fbl::Span<const uint8_t>(filler).subspan(0, length));
        });

    if (fill_result.is_error()) {
      fprintf(stderr, "%s\n", fill_result.error().c_str());
      return -1;
    }

    return 0;
  }

  // If length was not specified, use remainder of file after offset.
  // get_disk_size may return 0 due to MTD behavior with fstat.
  // This scenario is checked in the pave section below.
  if (length == 0 && disk_type != DiskType::Mtd) {
    length = get_disk_size(path, offset);
  }

  if (disk_type == DiskType::Mtd) {
    if (strcmp(command, "pave")) {
      fprintf(stderr, "Only the pave command is supported for MTD.\n");
      return -1;
    }

    if (!is_max_bad_blocks_set) {
      fprintf(stderr, "--max-bad-blocks is required when paving to MTD.\n");
      return -1;
    }
  }

  if (!strcmp(command, "create")) {
    // If length was specified, an offset was not, we were asked to create a
    // file, and the file does not exist, truncate it to the given length.
    if (length != 0 && offset == 0) {
      fbl::unique_fd fd(open(path, O_CREAT | O_EXCL | O_WRONLY, 0644));

      if (fd) {
        ftruncate(fd.get(), length);
      }
    }

    std::unique_ptr<FvmContainer> fvmContainer;
    if (FvmContainer::CreateNew(path, slice_size, offset, length, &fvmContainer) != ZX_OK) {
      fprintf(stderr, "Failed to create FVM container\n");
      return -1;
    }

    if (add_partitions(fvmContainer.get(), argc - i, argv + i) < 0) {
      return -1;
    }

    if (fvmContainer->Commit() != ZX_OK) {
      return -1;
    }

    if (resize_image_file_to_fit) {
      if (auto status = fvmContainer->ResizeImageFileToFit(); status != ZX_OK) {
        return status;
      }
    }

    if (convert_to_android_sparse_format) {
      if (fvmContainer->ConvertToAndroidSparseImage() != ZX_OK) {
        return -1;
      }
    }

    if (flags & fvm::kSparseFlagLz4) {
      if (fvmContainer->CompressWithLZ4() != ZX_OK) {
        return -1;
      }
    }
  } else if (!strcmp(command, "add")) {
    std::unique_ptr<FvmContainer> fvmContainer;
    if (FvmContainer::CreateExisting(path, offset, &fvmContainer) != ZX_OK) {
      return -1;
    }

    if (add_partitions(fvmContainer.get(), argc - i, argv + i) < 0) {
      return -1;
    }

    if (fvmContainer->Commit() != ZX_OK) {
      return -1;
    }
  } else if (!strcmp(command, "extend")) {
    if (length == 0 || offset > 0) {
      usage();
    }

    size_t disk_size = get_disk_size(path, 0);

    if (length <= disk_size) {
      fprintf(stderr, "Cannot extend to a value %zu less than current size %zu\n", length,
              disk_size);
      usage();
    }

    std::unique_ptr<FvmContainer> fvmContainer;
    if (FvmContainer::CreateExisting(path, offset, &fvmContainer) != ZX_OK) {
      return -1;
    }

    if (length_is_lower_bound) {
      fvmContainer->SetExtendLengthType(FvmContainer::ExtendLengthType::LOWER_BOUND);
    }

    if (fvmContainer->Extend(length) != ZX_OK) {
      return -1;
    }

    if (resize_image_file_to_fit) {
      if (auto status = fvmContainer->ResizeImageFileToFit(); status != ZX_OK) {
        return status;
      }
    }
  } else if (!strcmp(command, "sparse")) {
    if (offset) {
      fprintf(stderr, "Invalid sparse flags\n");
      return -1;
    }

    std::unique_ptr<SparseContainer> sparseContainer;
    if (SparseContainer::CreateNew(path, slice_size, flags, max_disk_size, &sparseContainer) !=
        ZX_OK) {
      return -1;
    }

    if (add_partitions(sparseContainer.get(), argc - i, argv + i) < 0) {
      return -1;
    }

    if (sparseContainer->Commit() != ZX_OK) {
      return -1;
    }
  } else if (!strcmp(command, "verify")) {
    std::unique_ptr<Container> container_data;
    if (Container::Create(path, offset, flags, &container_data) != ZX_OK) {
      return -1;
    }

    zx_status_t status = container_data->Verify();
    if (status != ZX_OK) {
      fprintf(stderr, "Verification failed: %d\n", status);
      return -1;
    }
  } else if (!strcmp(command, "decompress")) {
    if (argc - i != 2) {
      usage();
      return -1;
    }

    const char* input_type = argv[i];
    const char* input_path = argv[i + 1];

    if (!strcmp(input_type, "--default")) {
      if (!(input_type = DetermineImageInputTypeOption(input_path, offset))) {
        fprintf(stderr, "Fail to detect input file format\n");
        return -1;
      }
    }

    if (!strcmp(input_type, "--sparse")) {
      std::unique_ptr<SparseContainer> compressedContainer;
      if (SparseContainer::CreateExisting(input_path, &compressedContainer) != ZX_OK) {
        return -1;
      }

      if (compressedContainer->Decompress(path) != ZX_OK) {
        return -1;
      }

      std::unique_ptr<SparseContainer> sparseContainer;
      if (SparseContainer::CreateExisting(path, &sparseContainer) != ZX_OK) {
        return -1;
      }

      if (sparseContainer->Verify() != ZX_OK) {
        return -1;
      }
    } else if (!strcmp(input_type, "--lz4")) {
      if (fvm::SparseReader::DecompressLZ4File(input_path, path) != ZX_OK) {
        return -1;
      }
    } else if (!strcmp(input_type, "--raw")) {
      if (CopyFile(path, input_path) != ZX_OK) {
        return -1;
      }
    } else {
      usage();
      return -1;
    }
  } else if (!strcmp(command, "size")) {
    std::unique_ptr<SparseContainer> sparseContainer;
    if (SparseContainer::CreateExisting(path, &sparseContainer) != ZX_OK) {
      return -1;
    }

    if (disk_size == 0) {
      printf("%" PRIu64 "\n", sparseContainer->CalculateDiskSize());
    } else if (sparseContainer->CheckDiskSize(disk_size) != ZX_OK) {
      fprintf(stderr, "Sparse container will not fit in target disk size\n");
      return -1;
    }
  } else if (!strcmp(command, "used-data-size")) {
    std::unique_ptr<SparseContainer> sparseContainer;
    if (SparseContainer::CreateExisting(path, &sparseContainer) != ZX_OK) {
      return -1;
    }

    uint64_t size;

    if (sparseContainer->UsedDataSize(&size) != ZX_OK) {
      return -1;
    }
    printf("%" PRIu64 "\n", size);
  } else if (!strcmp(command, "used-inodes")) {
    std::unique_ptr<SparseContainer> sparseContainer;
    if (SparseContainer::CreateExisting(path, &sparseContainer) != ZX_OK) {
      return -1;
    }

    uint64_t used_inodes;

    if (sparseContainer->UsedInodes(&used_inodes) != ZX_OK) {
      return -1;
    }
    printf("%" PRIu64 "\n", used_inodes);
  } else if (!strcmp(command, "used-size")) {
    std::unique_ptr<SparseContainer> sparseContainer;
    if (SparseContainer::CreateExisting(path, &sparseContainer) != ZX_OK) {
      return -1;
    }

    uint64_t size;

    if (sparseContainer->UsedSize(&size) != ZX_OK) {
      return -1;
    }
    printf("%" PRIu64 "\n", size);
  } else if (!strcmp(command, "pave")) {
    char* input_type = argv[i];
    char* input_path = argv[i + 1];

    if (strcmp(input_type, "--sparse")) {
      fprintf(stderr, "pave command only accepts --sparse input option\n");
      usage();
      return -1;
    }

    std::unique_ptr<SparseContainer> sparseData;
    if (SparseContainer::CreateExisting(input_path, &sparseData) != ZX_OK) {
      return -1;
    }

    std::unique_ptr<fvm::host::FileWrapper> wrapper;

    if (disk_type == DiskType::File) {
      std::unique_ptr<fvm::host::UniqueFdWrapper> unique_fd_wrapper;
      if (fvm::host::UniqueFdWrapper::Open(path, O_CREAT | O_WRONLY, 0644, &unique_fd_wrapper) !=
          ZX_OK) {
        return -1;
      }

      wrapper = std::move(unique_fd_wrapper);
    } else if (disk_type == DiskType::Mtd) {
      zx_status_t status =
          CreateFileWrapperFromMtd(path, safemath::saturated_cast<uint32_t>(offset),
                                   safemath::saturated_cast<uint32_t>(max_bad_blocks), &wrapper);

      if (status != ZX_OK) {
        return -1;
      }

      // The byte offset into the output file is handled by CreateFileWrapperFromMtd.
      offset = 0;

      // Length may be 0 at this point if the user did not specify a size.
      // Use all of the space reported by the FTL in this case.
      if (length == 0) {
        length = wrapper->Size();
      }
    } else {
      fprintf(stderr, "Unknown disk type\n");
      return -1;
    }

    if (sparseData->Pave(std::move(wrapper), offset, length) != ZX_OK) {
      return -1;
    }
  } else {
    fprintf(stderr, "Unrecognized command: \"%s\"\n", command);
    usage();
  }

  return 0;
}
