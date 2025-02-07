// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/fvm/host/sparse_container.h"

#include <fcntl.h>
#include <inttypes.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <memory>
#include <sstream>
#include <utility>
#include <vector>

#include <safemath/checked_math.h>

#include "src/storage/fvm/format.h"
#include "src/storage/fvm/fvm_sparse.h"
#include "src/storage/fvm/host/format.h"
#include "src/storage/fvm/host/internal_snapshot_meta_format.h"
#include "src/storage/fvm/host/sparse_paver.h"
#include "src/storage/minfs/format.h"

constexpr size_t kLz4HeaderSize = 15;

static LZ4F_preferences_t lz4_prefs = {
    .frameInfo =
        {
            .blockSizeID = LZ4F_max64KB,
            .blockMode = LZ4F_blockIndependent,
        },
    .compressionLevel = 0,
};

fit::result<CompressionContext, std::string> CompressionContext::Create() {
  CompressionContext context;
  LZ4F_errorCode_t errc = LZ4F_createCompressionContext(&context.cctx_, LZ4F_VERSION);
  if (LZ4F_isError(errc)) {
    std::ostringstream stream;
    stream << "Could not create compression context: " << LZ4F_getErrorName(errc) << "\n";
    return fit::error(stream.str());
  }
  return fit::ok(std::move(context));
}

zx_status_t CompressionContext::Setup(size_t max_len) {
  Reset(kLz4HeaderSize + LZ4F_compressBound(max_len, &lz4_prefs));

  size_t r = LZ4F_compressBegin(cctx_, GetBuffer(), GetRemaining(), &lz4_prefs);
  if (LZ4F_isError(r)) {
    fprintf(stderr, "Could not begin compression: %s\n", LZ4F_getErrorName(r));
    return ZX_ERR_INTERNAL;
  }

  IncreaseOffset(r);
  return ZX_OK;
}

zx_status_t CompressionContext::Compress(const void* data, size_t length) {
  size_t r = LZ4F_compressUpdate(cctx_, GetBuffer(), GetRemaining(), data, length, NULL);
  if (LZ4F_isError(r)) {
    fprintf(stderr, "Could not compress data: %s\n", LZ4F_getErrorName(r));
    return ZX_ERR_INTERNAL;
  }
  IncreaseOffset(r);
  return ZX_OK;
}

zx_status_t CompressionContext::Finish() {
  zx_status_t result = ZX_OK;

  size_t r = LZ4F_compressEnd(cctx_, GetBuffer(), GetRemaining(), NULL);
  if (LZ4F_isError(r)) {
    fprintf(stderr, "Could not finish compression: %s\n", LZ4F_getErrorName(r));
    result = ZX_ERR_INTERNAL;
  } else {
    IncreaseOffset(r);
  }
  return result;
}

zx_status_t SparseContainer::CreateNew(const char* path, size_t slice_size, uint32_t flags,
                                       std::unique_ptr<SparseContainer>* out) {
  return CreateNew(path, slice_size, flags, 0, out);
}

zx_status_t SparseContainer::CreateNew(const char* path, size_t slice_size, uint32_t flags,
                                       uint64_t max_disk_size,
                                       std::unique_ptr<SparseContainer>* out) {
  std::unique_ptr<SparseContainer> sparseContainer(new SparseContainer(path, slice_size, flags));
  zx_status_t status;
  if ((status = sparseContainer->InitNew()) != ZX_OK) {
    return status;
  }
  sparseContainer->image_.maximum_disk_size = max_disk_size;
  *out = std::move(sparseContainer);
  return ZX_OK;
}

zx_status_t SparseContainer::CreateExisting(const char* path,
                                            std::unique_ptr<SparseContainer>* out) {
  std::unique_ptr<SparseContainer> sparseContainer(new SparseContainer(path, 0, 0));

  zx_status_t status;
  if ((status = sparseContainer->InitExisting()) != ZX_OK) {
    return status;
  }

  *out = std::move(sparseContainer);
  return ZX_OK;
}

SparseContainer::SparseContainer(const char* path, uint64_t slice_size, uint32_t flags)
    : Container(path, slice_size, flags), valid_(false), disk_size_(0), extent_size_(0) {}

SparseContainer::~SparseContainer() = default;

uint64_t SparseContainer::MaximumDiskSize() const {
  return (image_.maximum_disk_size == 0) ? disk_size_ : image_.maximum_disk_size;
}

zx_status_t SparseContainer::InitNew() {
  if (slice_size_ == 0) {
    fprintf(stderr, "Cannot initialize sparse container with no slice size\n");
    return ZX_ERR_BAD_STATE;
  }

  fd_.reset(open(path_.data(), O_CREAT | O_RDWR, 0666));

  if (!fd_) {
    fprintf(stderr, "Failed to open sparse data path\n");
    return ZX_ERR_IO;
  }

  image_.magic = fvm::kSparseFormatMagic;
  image_.version = fvm::kSparseFormatVersion;
  image_.slice_size = slice_size_;
  image_.partition_count = 0;
  image_.maximum_disk_size = 0;
  image_.header_length = sizeof(fvm::SparseImage);
  image_.flags = flags_;
  partitions_.reset();
  dirty_ = true;
  valid_ = true;
  extent_size_ = 0;

  auto result = CompressionContext::Create();
  if (!result.is_ok()) {
    fprintf(stderr, "%s", result.take_error_result().error.c_str());
    return ZX_ERR_INTERNAL;
  }
  compression_ = std::move(result.take_ok_result().value);

  xprintf("Initialized new sparse data container.\n");
  return ZX_OK;
}

zx_status_t SparseContainer::InitExisting() {
  fd_.reset(open(path_.data(), O_RDWR, 0666));

  if (!fd_) {
    fprintf(stderr, "Failed to open sparse data path\n");
    return ZX_ERR_IO;
  }

  struct stat s;
  if (fstat(fd_.get(), &s) < 0) {
    fprintf(stderr, "Failed to stat %s\n", path_.data());
    return ZX_ERR_IO;
  }

  if (s.st_size == 0) {
    return ZX_ERR_BAD_STATE;
  }

  disk_size_ = s.st_size;

  fbl::unique_fd dup_fd(dup(fd_.get()));
  zx_status_t status = fvm::SparseReader::CreateSilent(std::move(dup_fd), &reader_);
  if (status != ZX_OK) {
    fprintf(stderr, "SparseContainer: Failed to read metadata from sparse file\n");
    return status;
  }

  memcpy(&image_, reader_->Image(), sizeof(fvm::SparseImage));
  flags_ = image_.flags;
  slice_size_ = image_.slice_size;
  extent_size_ = disk_size_ - image_.header_length;

  uintptr_t partition_ptr = reinterpret_cast<uintptr_t>(reader_->Partitions());
  for (unsigned i = 0; i < image_.partition_count; i++) {
    SparsePartitionInfo partition;
    memcpy(&partition.descriptor, reinterpret_cast<void*>(partition_ptr),
           sizeof(fvm::PartitionDescriptor));
    partitions_.push_back(std::move(partition));
    partition_ptr += sizeof(fvm::PartitionDescriptor);

    for (size_t j = 0; j < partitions_[i].descriptor.extent_count; j++) {
      fvm::ExtentDescriptor extent;
      memcpy(&extent, reinterpret_cast<void*>(partition_ptr), sizeof(fvm::ExtentDescriptor));
      partitions_[i].extents.push_back(extent);
      partition_ptr += sizeof(fvm::ExtentDescriptor);
    }
  }
  auto result = CompressionContext::Create();
  if (!result.is_ok()) {
    fprintf(stderr, "%s", result.take_error_result().error.c_str());
    return ZX_ERR_INTERNAL;
  }
  compression_ = std::move(result.take_ok_result().value);
  valid_ = true;
  xprintf("Successfully read from existing sparse data container.\n");
  return ZX_OK;
}

zx_status_t SparseContainer::Verify() const {
  CheckValid();

  if (image_.flags & fvm::kSparseFlagLz4) {
    // Decompression must occur before verification, since all contents must be available for
    // fsck.
    fprintf(stderr,
            "SparseContainer: Found compressed container; contents cannot be"
            " verified\n");
    return ZX_ERR_INVALID_ARGS;
  }

  if (image_.magic != fvm::kSparseFormatMagic) {
    fprintf(stderr, "SparseContainer: Bad magic\n");
    return ZX_ERR_IO;
  }

  xprintf("Slice size is %" PRIu64 "\n", image_.slice_size);
  xprintf("Found %" PRIu64 " partitions\n", image_.partition_count);

  off_t start = 0;
  off_t end = image_.header_length;
  for (unsigned i = 0; i < image_.partition_count; i++) {
    fbl::Vector<size_t> extent_lengths;
    start = end;
    xprintf("Found partition %u with %u extents\n", i, partitions_[i].descriptor.extent_count);

    if (partitions_[i].descriptor.flags & fvm::kSparseFlagSnapshotMetadataPartition) {
      // Reserve partitions need no verification.
      continue;
    }

    for (unsigned j = 0; j < partitions_[i].descriptor.extent_count; j++) {
      extent_lengths.push_back(partitions_[i].extents[j].extent_length);
      end += partitions_[i].extents[j].extent_length;
      xprintf("\tExtent[%u]: slice_start: %" PRIu64 ". slice_count: %" PRIu64 "\n", j,
              partitions_[i].extents[j].slice_start, partitions_[i].extents[j].slice_count);
    }

    zx_status_t status;
    disk_format_t part;
    if ((status = Format::Detect(fd_.get(), start, &part)) != ZX_OK) {
      return status;
    }

    fbl::unique_fd dupfd(dup(fd_.get()));
    if (!dupfd) {
      fprintf(stderr, "Failed to duplicate fd\n");
      return ZX_ERR_INTERNAL;
    }

    if ((status = Format::Check(std::move(dupfd), start, end, extent_lengths, part)) != ZX_OK) {
      const char* name = reinterpret_cast<const char*>(partitions_[i].descriptor.name);
      fprintf(stderr, "%s fsck returned an error.\n", name);
      return status;
    }
  }

  if (end < 0 || static_cast<size_t>(end) != disk_size_) {
    fprintf(stderr,
            "Header + extent sizes (%" PRIu64
            ") do not match sparse file size "
            "(%zu)\n",
            end, disk_size_);
    return ZX_ERR_IO_DATA_INTEGRITY;
  }

  return ZX_OK;
}

// TODO(auradkar): Iteration over partition is copy pasted several times in this file.
//                 Iteration can be made more common code.
zx_status_t SparseContainer::PartitionsIterator(UsedSize_f* used_size_f, uint64_t* out_size) const {
  uint64_t total_size = 0;
  uint64_t size = 0;

  CheckValid();

  if (image_.flags & fvm::kSparseFlagLz4) {
    // Decompression must occur before verification, since all contents must be available
    // reading superblock.
    fprintf(stderr,
            "SparseContainer: Found compressed container; contents cannot be"
            " read\n");
    return ZX_ERR_INVALID_ARGS;
  }

  if (image_.magic != fvm::kSparseFormatMagic) {
    fprintf(stderr, "SparseContainer: Bad magic\n");
    return ZX_ERR_IO;
  }

  xprintf("Slice size is %" PRIu64 "\n", image_.slice_size);
  xprintf("Found %" PRIu64 " partitions\n", image_.partition_count);

  off_t start = 0;
  off_t end = image_.header_length;
  for (unsigned i = 0; i < image_.partition_count; i++) {
    fbl::Vector<size_t> extent_lengths;
    start = end;
    xprintf("Found partition %u with %u extents\n", i, partitions_[i].descriptor.extent_count);

    for (unsigned j = 0; j < partitions_[i].descriptor.extent_count; j++) {
      extent_lengths.push_back(partitions_[i].extents[j].extent_length);
      end += partitions_[i].extents[j].extent_length;
    }

    zx_status_t status;
    disk_format_t part;
    if ((status = Format::Detect(fd_.get(), start, &part)) != ZX_OK) {
      return status;
    }

    if ((status = used_size_f(fd_, start, end, extent_lengths, part, &size)) != ZX_OK) {
      const char* name = reinterpret_cast<const char*>(partitions_[i].descriptor.name);
      fprintf(stderr, "%s used_size returned an error.\n", name);
      return status;
    }
    total_size += size;
  }

  *out_size = total_size;
  return ZX_OK;
}

zx_status_t SparseContainer::UsedDataSize(uint64_t* out_size) const {
  return PartitionsIterator(Format::UsedDataSize, out_size);
}

zx_status_t SparseContainer::UsedInodes(uint64_t* out_inodes) const {
  return PartitionsIterator(Format::UsedInodes, out_inodes);
}

zx_status_t SparseContainer::UsedSize(uint64_t* out_size) const {
  return PartitionsIterator(Format::UsedSize, out_size);
}

zx_status_t SparseContainer::CheckDiskSize(uint64_t target_disk_size) const {
  CheckValid();

  fvm::Header fvm_header = GetFvmConfiguration(target_disk_size);
  size_t usable_slices = fvm_header.GetAllocationTableAllocatedEntryCount();
  size_t required_slices = SliceCount();

  if (usable_slices < required_slices) {
    return ZX_ERR_OUT_OF_RANGE;
  }

  // Compute the header representing the required slices.
  fvm_header.SetSliceCount(required_slices);
  if (target_disk_size < fvm_header.fvm_partition_size)
    return ZX_ERR_OUT_OF_RANGE;

  return ZX_OK;
}

uint64_t SparseContainer::CalculateDiskSize() const {
  CheckValid();
  return CalculateDiskSizeForSlices(SliceCount());
}

zx_status_t SparseContainer::Commit() {
  if (!dirty_ || image_.partition_count == 0) {
    fprintf(stderr, "Commit: Nothing to write.\n");
    return ZX_OK;
  }

  // Reset file length to 0
  if (ftruncate(fd_.get(), 0) != 0) {
    fprintf(stderr, "Failed to truncate fvm container");
    return ZX_ERR_IO;
  }

  // Recalculate and verify header length
  uint64_t header_length = 0;

  if (lseek(fd_.get(), 0, SEEK_SET) < 0) {
    fprintf(stderr, "Seek reset failed\n");
    return ZX_ERR_IO;
  }

  header_length += sizeof(fvm::SparseImage);
  if (image_.flags & fvm::kSparseFlagLz4) {
    image_.flags |= fvm::kSparseFlagZeroFillNotRequired;
  }
  if (write(fd_.get(), &image_, sizeof(fvm::SparseImage)) != sizeof(fvm::SparseImage)) {
    fprintf(stderr, "Write sparse image header failed\n");
    return ZX_ERR_IO;
  }

  for (unsigned i = 0; i < image_.partition_count; i++) {
    fvm::PartitionDescriptor partition = partitions_[i].descriptor;

    header_length += sizeof(fvm::PartitionDescriptor);
    if (write(fd_.get(), &partition, sizeof(fvm::PartitionDescriptor)) !=
        sizeof(fvm::PartitionDescriptor)) {
      fprintf(stderr, "Write partition failed\n");
      return ZX_ERR_IO;
    }

    Format* format = nullptr;
    if ((flags_ & fvm::kSparseFlagLz4) && !(partition.flags & fvm::kSparseFlagCorrupted)) {
      format = partitions_[i].format.get();
    }
    // Write out each extent in the partition
    for (unsigned j = 0; j < partition.extent_count; j++) {
      fvm::ExtentDescriptor& extent = partitions_[i].extents[j];
      header_length += sizeof(fvm::ExtentDescriptor);
      // If format is non-null, then we should zero fill if the slice requests it.
      if (format) {
        auto extent_or = format->GetExtent(j);
        if (extent_or.is_error()) {
          fprintf(stderr, "Unable to access partition extent\n");
          return extent_or.status_value();
        }
        if (extent_or.value().zero_fill) {
          extent.extent_length = extent.slice_count * slice_size_;
        }
      }
      if (write(fd_.get(), &extent, sizeof(fvm::ExtentDescriptor)) !=
          sizeof(fvm::ExtentDescriptor)) {
        fprintf(stderr, "Write extent failed\n");
        return ZX_ERR_IO;
      }
    }
  }

  if (header_length != image_.header_length) {
    fprintf(stderr, "Header length does not match!\n");
    return ZX_ERR_INTERNAL;
  }

  zx_status_t status;
  if ((status = PrepareWrite(extent_size_)) != ZX_OK) {
    return status;
  }

  // Write each partition out to sparse file
  for (unsigned i = 0; i < image_.partition_count; i++) {
    fvm::PartitionDescriptor partition = partitions_[i].descriptor;
    Format* format = partitions_[i].format.get();
    // Write out each extent in the partition
    for (unsigned j = 0; j < partition.extent_count; j++) {
      if (!format) {
        // Zero-fill if there is no format to instruct how to fill the data.
        if (zx_status_t status = WriteZeroes(partitions_[i].extents[j].extent_length);
            status != ZX_OK) {
          return status;
        }
        continue;
      }
      auto extent_or = format->GetExtent(j);
      if (extent_or.is_error()) {
        fprintf(stderr, "Unable to access partition extent\n");
        return extent_or.status_value();
      }
      const ExtentInfo& extent = extent_or.value();

      // Write out each block in the extent
      size_t bytes_written = 0;
      for (unsigned k = 0; k < extent.vslice_count * format->BlocksPerSlice(); ++k) {
        if (k >= extent.block_count) {
          // Zero fill, but only if compression is enabled and it has been requested; we wrote an
          // appropriate extent entry earlier.
          if (!(flags_ & fvm::kSparseFlagLz4) || !extent.zero_fill) {
            break;
          }
          format->EmptyBlock();
        } else if (format->FillBlock(extent.block_offset + k) != ZX_OK) {
          fprintf(stderr, "Failed to read block\n");
          return ZX_ERR_IO;
        }
        bytes_written += format->BlockSize();

        if (WriteData(format->Data(), format->BlockSize()) != ZX_OK) {
          fprintf(stderr, "Failed to write data to sparse file\n");
          return ZX_ERR_IO;
        }
      }
      ZX_ASSERT(bytes_written == partitions_[i].extents[j].extent_length);
    }
  }

  if ((status = CompleteWrite()) != ZX_OK) {
    return status;
  }

  struct stat s;
  if (fstat(fd_.get(), &s) < 0) {
    fprintf(stderr, "Failed to stat container\n");
    return ZX_ERR_IO;
  }

  disk_size_ = s.st_size;
  if (image_.maximum_disk_size > 0 && disk_size_ > image_.maximum_disk_size) {
    fprintf(stderr, "FVM image disk_size exceeds maximum allowed size.");
    return ZX_ERR_NO_SPACE;
  }

  xprintf("Successfully wrote sparse data to disk.\n");
  return ZX_OK;
}

zx_status_t SparseContainer::Pave(std::unique_ptr<fvm::host::FileWrapper> wrapper,
                                  size_t disk_offset, size_t disk_size) {
  uint64_t minimum_disk_size = CalculateDiskSize();
  uint64_t target_size = disk_size;

  if (disk_size == 0) {
    disk_size = minimum_disk_size;
    target_size = disk_size;
  }

  // Prefer using the sparse container's maximum disk size if available.
  if (image_.maximum_disk_size > 0) {
    target_size = image_.maximum_disk_size;
  }

  // Truncate file to size the caller expects. Some files wrapped by FileWrapper may not support
  // truncate, e.g. block devices.
  zx_status_t status = wrapper->Truncate(disk_offset + disk_size);
  if (status != ZX_OK && status != ZX_ERR_NOT_SUPPORTED) {
    return status;
  }

  uint64_t wrapper_size = static_cast<uint64_t>(wrapper->Size());
  if (wrapper_size < disk_offset + minimum_disk_size) {
    fprintf(stderr,
            "Cannot pave %" PRIu64 " bytes at offset %zu to FileWrapper of size %" PRIu64
            " bytes\n",
            minimum_disk_size, disk_offset, wrapper_size);
    return ZX_ERR_INVALID_ARGS;
  }

  std::unique_ptr<SparsePaver> paver;
  status = SparsePaver::Create(std::move(wrapper), slice_size_, disk_offset, target_size, &paver);

  if (status != ZX_OK) {
    fprintf(stderr, "Failed to create SparsePaver\n");
    return status;
  }

  for (uint32_t i = 0; i < image_.partition_count; i++) {
    if ((partitions_[i].descriptor.flags & fvm::kSparseFlagZxcrypt) != 0) {
      // TODO(planders): Remove this error when we can create zxcrypt'd FVMs on host.
      printf("SparseContainer::Pave: zxcrypt not yet implemented for host-side FVM\n");
      return ZX_ERR_NOT_SUPPORTED;
    }

    if ((status = paver->AddPartition(&partitions_[i], reader_.get())) != ZX_OK) {
      return status;
    }
  }

  return paver->Commit();
}

size_t SparseContainer::SliceSize() const { return image_.slice_size; }

size_t SparseContainer::SliceCount() const {
  CheckValid();
  size_t slices = 0;

  for (unsigned i = 0; i < image_.partition_count; i++) {
    if ((partitions_[i].descriptor.flags & fvm::kSparseFlagZxcrypt) != 0) {
      slices += kZxcryptExtraSlices;
    }

    for (unsigned j = 0; j < partitions_[i].descriptor.extent_count; j++) {
      slices += partitions_[i].extents[j].slice_count;
    }
  }

  return slices;
}

zx_status_t SparseContainer::AddCorruptedPartition(const char* type, uint64_t target_size) {
  if (strcmp(kDataTypeName, type) != 0) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  uint64_t partition_index = image_.partition_count;
  SparsePartitionInfo info;
  auto& descriptor = info.descriptor;
  info.format = nullptr;
  descriptor.magic = fvm::kPartitionDescriptorMagic;
  memcpy(descriptor.type, kDataType, sizeof(kDataType));
  memcpy(descriptor.name, kMinfsName, sizeof(kMinfsName));
  // For the current use case we dont want to mark it as zxcrypt,
  // reformat path will update this to be encrypted.
  descriptor.flags = fvm::kSparseFlagCorrupted;
  descriptor.extent_count = 0;

  image_.header_length += sizeof(fvm::PartitionDescriptor);
  partitions_.push_back(std::move(info));
  image_.partition_count++;
  zx_status_t status = ZX_OK;

  // Allocate two slices to account for zxcrypt.
  fvm::ExtentDescriptor extent{
      .magic = fvm::kExtentDescriptorMagic,
      .slice_start = 0,
      .slice_count = 2,
      .extent_length = minfs::kMinfsBlockSize,
  };
  if ((status = AllocateExtent(static_cast<uint32_t>(partition_index), extent)) != ZX_OK) {
    return status;
  }
  return status;
}

zx_status_t SparseContainer::AddPartition(const char* path, const char* type_name,
                                          FvmReservation* reserve) {
  std::unique_ptr<Format> format;
  zx_status_t status;

  if ((status = Format::Create(path, type_name, &format)) != ZX_OK) {
    fprintf(stderr, "Failed to initialize partition\n");
    return status;
  }

  if ((status = AllocatePartition(std::move(format), reserve)) != ZX_OK) {
    return status;
  }

  return ZX_OK;
}

zx_status_t SparseContainer::AddSnapshotMetadataPartition(size_t reserved_slices) {
  uint64_t partition_index = image_.partition_count;
  fvm::VPartitionEntry entry = fvm::VPartitionEntry::CreateSnapshotMetadataPartition();
  SparsePartitionInfo info;
  auto& descriptor = info.descriptor;
  info.format = nullptr;
  descriptor.magic = fvm::kPartitionDescriptorMagic;
  memcpy(descriptor.type, entry.type, sizeof(kDataType));
  memcpy(descriptor.name, entry.unsafe_name, sizeof(descriptor.name));
  descriptor.flags = fvm::kSparseFlagSnapshotMetadataPartition;
  descriptor.extent_count = 0;

  // TODO(fxbug.dev/59567): Add partition/extent entries describing blobfs.
  std::vector<fvm::PartitionSnapshotState> partition_states{};
  std::vector<fvm::SnapshotExtentType> extent_types{};
  info.format = std::make_unique<InternalSnapshotMetaFormat>(reserved_slices, slice_size_,
                                                             partition_states, extent_types);
  // Find out the actual number of slices we need by asking |format|.
  uint32_t final_slices;
  if (zx_status_t status = info.format->GetSliceCount(&final_slices); status != ZX_OK) {
    return status;
  }

  fvm::ExtentDescriptor extent{
      .magic = fvm::kExtentDescriptorMagic,
      .slice_start = 0u,
      .slice_count = final_slices,
      .extent_length = info.format->BlockSize(),
  };

  image_.header_length += sizeof(fvm::PartitionDescriptor);
  partitions_.push_back(std::move(info));
  image_.partition_count++;

  return AllocateExtent(static_cast<uint32_t>(partition_index), extent);
}

zx_status_t SparseContainer::Decompress(const char* path) {
  if ((flags_ & fvm::kSparseFlagLz4) == 0) {
    fprintf(stderr, "Cannot decompress un-compressed sparse file\n");
    return ZX_ERR_NOT_SUPPORTED;
  }

  fbl::unique_fd fd;

  fd.reset(open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644));
  if (!fd) {
    fprintf(stderr, "could not open %s: %s\n", path, strerror(errno));
    return ZX_ERR_IO;
  }

  return reader_->WriteDecompressed(std::move(fd));
}

zx_status_t SparseContainer::AllocatePartition(std::unique_ptr<Format> format,
                                               FvmReservation* reserve) {
  SparsePartitionInfo partition;
  format->GetPartitionInfo(&partition.descriptor);
  partition.descriptor.magic = fvm::kPartitionDescriptorMagic;
  partition.descriptor.extent_count = 0;
  image_.header_length += sizeof(fvm::PartitionDescriptor);
  uint32_t part_index = safemath::checked_cast<uint32_t>(image_.partition_count);

  zx_status_t status;
  if ((status = format->MakeFvmReady(SliceSize(), part_index, reserve)) != ZX_OK) {
    return status;
  }

  partitions_.push_back(std::move(partition));

  if (++image_.partition_count != partitions_.size()) {
    fprintf(stderr, "Unexpected number of partitions\n");
    return ZX_ERR_INTERNAL;
  }

  unsigned i = 0;
  uint64_t extent_length;

  while (true) {
    auto extent_or = format->GetExtent(i++);
    if (extent_or.is_error()) {
      if (extent_or.status_value() == ZX_ERR_OUT_OF_RANGE) {
        break;
      }
      return extent_or.status_value();
    }
    if (mul_overflow(extent_or.value().block_count, format->BlockSize(), &extent_length)) {
      fprintf(stderr, "Multiplication overflow when getting extent length\n");
      return ZX_ERR_OUT_OF_RANGE;
    }
    fvm::ExtentDescriptor extent{
        .magic = fvm::kExtentDescriptorMagic,
        .slice_start = extent_or.value().vslice_start,
        .slice_count = extent_or.value().vslice_count,
        .extent_length = extent_length,
    };
    if ((status = AllocateExtent(part_index, extent)) != ZX_OK) {
      return status;
    }
  }

  partitions_[part_index].format = std::move(format);
  return ZX_OK;
}

zx_status_t SparseContainer::AllocateExtent(uint32_t part_index, fvm::ExtentDescriptor extent) {
  if (part_index >= image_.partition_count) {
    fprintf(stderr, "Partition is not yet allocated\n");
    return ZX_ERR_OUT_OF_RANGE;
  }

  ZX_ASSERT(extent.magic == fvm::kExtentDescriptorMagic);
  ZX_ASSERT(slice_size_ == image_.slice_size);
  ZX_ASSERT((extent.slice_count * image_.slice_size) >= extent.extent_length);
  SparsePartitionInfo* partition = &partitions_[part_index];
  partition->extents.push_back(extent);

  if (partition->extents.size() != ++partition->descriptor.extent_count) {
    fprintf(stderr, "Unexpected number of extents\n");
    return ZX_ERR_INTERNAL;
  }

  image_.header_length += sizeof(fvm::ExtentDescriptor);
  extent_size_ += extent.extent_length;
  dirty_ = true;
  return ZX_OK;
}

zx_status_t SparseContainer::PrepareWrite(size_t max_len) {
  if ((flags_ & fvm::kSparseFlagLz4) == 0) {
    return ZX_OK;
  }

  return compression_.Setup(max_len);
}

zx_status_t SparseContainer::WriteData(const void* data, size_t length) {
  if ((flags_ & fvm::kSparseFlagLz4) != 0) {
    return compression_.Compress(data, length);
  }

  ssize_t result = write(fd_.get(), data, length);
  if (result < 0 || static_cast<size_t>(result) != length) {
    return ZX_ERR_IO;
  }

  return ZX_OK;
}

zx_status_t SparseContainer::WriteZeroes(size_t length) {
  constexpr std::array<uint8_t, fvm::kBlockSize> kBuffer = {0};
  while (length > 0) {
    size_t to_write = std::min(length, kBuffer.size());
    if (zx_status_t status = WriteData(kBuffer.data(), to_write); status != ZX_OK) {
      return status;
    }
    length -= to_write;
  }
  return ZX_OK;
}

zx_status_t SparseContainer::CompleteWrite() {
  if ((flags_ & fvm::kSparseFlagLz4) == 0) {
    return ZX_OK;
  }

  zx_status_t status = compression_.Finish();

  if (status != ZX_OK) {
    return status;
  }

  size_t remaining_length = compression_.GetLength();

  while (remaining_length > 0) {
    uintptr_t data_ptr = reinterpret_cast<uintptr_t>(compression_.GetData()) +
                         (compression_.GetLength() - remaining_length);
    ssize_t result = write(fd_.get(), reinterpret_cast<void*>(data_ptr), remaining_length);

    if (result <= 0 || static_cast<size_t>(result) > remaining_length) {
      fprintf(stderr, "Error occurred during sparse writeback: %s\n", strerror(errno));
      return ZX_ERR_IO;
    }

    remaining_length -= result;
  }

  return ZX_OK;
}

void SparseContainer::CheckValid() const {
  if (!valid_) {
    fprintf(stderr, "Error: Sparse container is invalid\n");
    exit(-1);
  }
}

fvm::Header SparseContainer::GetFvmConfiguration(uint64_t target_disk_size) const {
  return fvm::Header::FromDiskSize(fvm::kMaxUsablePartitions, target_disk_size, image_.slice_size);
}
