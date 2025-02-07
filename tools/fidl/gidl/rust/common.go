// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package rust

import (
	"bytes"
	"encoding/hex"
	"fmt"
	"strconv"
	"strings"

	gidlir "go.fuchsia.dev/fuchsia/tools/fidl/gidl/ir"
	gidlmixer "go.fuchsia.dev/fuchsia/tools/fidl/gidl/mixer"
	fidl "go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

func buildHandleDefs(defs []gidlir.HandleDef) string {
	if len(defs) == 0 {
		return ""
	}
	var builder strings.Builder
	builder.WriteString("[\n")
	for i, d := range defs {
		// Write indices corresponding to the .gidl file handle_defs block.
		builder.WriteString(fmt.Sprintf("HandleSubtype::%s, // #%d\n", handleTypeName(d.Subtype), i))
	}
	builder.WriteString("]")
	return builder.String()
}

func buildBytes(bytes []byte) string {
	var builder strings.Builder
	builder.WriteString("[\n")
	for i, b := range bytes {
		builder.WriteString(fmt.Sprintf("0x%02x,", b))
		if i%8 == 7 {
			builder.WriteString("\n")
		}
	}
	builder.WriteString("]")
	return builder.String()
}

func buildHandles(handles []gidlir.Handle) string {
	var builder strings.Builder
	builder.WriteString("[\n")
	for i, h := range handles {
		builder.WriteString(fmt.Sprintf("%d,", h))
		if i%8 == 7 {
			builder.WriteString("\n")
		}
	}
	builder.WriteString("]")
	return builder.String()
}

func buildHandleValues(handles []gidlir.Handle) string {
	var builder strings.Builder
	builder.WriteString("vec![\n")
	for _, h := range handles {
		builder.WriteString(fmt.Sprintf("%s,", buildHandleValue(h)))
	}
	builder.WriteString("]")
	return builder.String()
}

func buildUnknownData(data gidlir.UnknownData, isResource bool) string {
	if !isResource {
		return fmt.Sprintf("vec!%s", buildBytes(data.Bytes))
	}
	return fmt.Sprintf(
		"UnknownData { bytes: vec!%s, handles: %s }",
		buildBytes(data.Bytes),
		buildHandleValues(data.Handles))
}

func escapeStr(value string) string {
	var (
		buf    bytes.Buffer
		src    = []byte(value)
		dstLen = hex.EncodedLen(len(src))
		dst    = make([]byte, dstLen)
	)
	hex.Encode(dst, src)
	for i := 0; i < dstLen; i += 2 {
		buf.WriteString("\\x")
		buf.WriteByte(dst[i])
		buf.WriteByte(dst[i+1])
	}
	return buf.String()
}

func visit(value interface{}, decl gidlmixer.Declaration) string {
	switch value := value.(type) {
	case bool:
		return strconv.FormatBool(value)
	case int64, uint64, float64:
		switch decl := decl.(type) {
		case gidlmixer.PrimitiveDeclaration:
			suffix := primitiveTypeName(decl.Subtype())
			return fmt.Sprintf("%v%s", value, suffix)
		case *gidlmixer.BitsDecl:
			primitive := visit(value, &decl.Underlying)
			if decl.IsFlexible() {
				return fmt.Sprintf("%s::from_bits_allow_unknown(%v)", declName(decl), primitive)
			}
			// Use from_bits_unchecked so that encode_failure tests work. It's
			// not worth the effort to make the test type available here and use
			// from_bits(...).unwrap() in success cases, since all this would do
			// is move validation from the bindings to GIDL.
			return fmt.Sprintf("unsafe { %s::from_bits_unchecked(%v) }", declName(decl), primitive)
		case *gidlmixer.EnumDecl:
			primitive := visit(value, &decl.Underlying)
			if decl.IsFlexible() {
				return fmt.Sprintf("%s::from_primitive_allow_unknown(%v)", declName(decl), primitive)
			}
			return fmt.Sprintf("%s::from_primitive(%v).unwrap()", declName(decl), primitive)
		}
	case gidlir.RawFloat:
		switch decl.(*gidlmixer.FloatDecl).Subtype() {
		case fidl.Float32:
			return fmt.Sprintf("f32::from_bits(%#b)", value)
		case fidl.Float64:
			return fmt.Sprintf("f64::from_bits(%#b)", value)
		}
	case string:
		var expr string
		if fidl.PrintableASCII(value) {
			expr = fmt.Sprintf("String::from(%q)", value)
		} else {
			expr = fmt.Sprintf("std::str::from_utf8(b\"%s\").unwrap().to_string()", escapeStr(value))
		}
		return wrapNullable(decl, expr)
	case gidlir.Handle:
		expr := buildHandleValue(value)
		return wrapNullable(decl, expr)
	case gidlir.Record:
		switch decl := decl.(type) {
		case *gidlmixer.StructDecl:
			return onStruct(value, decl)
		case *gidlmixer.TableDecl:
			return onTable(value, decl)
		case *gidlmixer.UnionDecl:
			return onUnion(value, decl)
		}
	case []interface{}:
		switch decl := decl.(type) {
		case *gidlmixer.ArrayDecl:
			return onList(value, decl)
		case *gidlmixer.VectorDecl:
			return onList(value, decl)
		}
	case nil:
		if !decl.IsNullable() {
			panic(fmt.Sprintf("got nil for non-nullable type: %T", decl))
		}
		return "None"
	}
	panic(fmt.Sprintf("not implemented: %T", value))
}

func declName(decl gidlmixer.NamedDeclaration) string {
	return identifierName(decl.Name())
}

// TODO(fxbug.dev/39407): Move into a common library outside GIDL.
func identifierName(qualifiedName string) string {
	parts := strings.Split(qualifiedName, "/")
	lastPartsIndex := len(parts) - 1
	for i, part := range parts {
		if i == lastPartsIndex {
			parts[i] = fidl.ToUpperCamelCase(part)
		} else {
			parts[i] = fidl.ToSnakeCase(part)
		}
	}
	return strings.Join(parts, "::")
}

func primitiveTypeName(subtype fidl.PrimitiveSubtype) string {
	switch subtype {
	case fidl.Bool:
		return "bool"
	case fidl.Int8:
		return "i8"
	case fidl.Uint8:
		return "u8"
	case fidl.Int16:
		return "i16"
	case fidl.Uint16:
		return "u16"
	case fidl.Int32:
		return "i32"
	case fidl.Uint32:
		return "u32"
	case fidl.Int64:
		return "i64"
	case fidl.Uint64:
		return "u64"
	case fidl.Float32:
		return "f32"
	case fidl.Float64:
		return "f64"
	default:
		panic(fmt.Sprintf("unexpected subtype %v", subtype))
	}
}

func handleTypeName(subtype fidl.HandleSubtype) string {
	switch subtype {
	case fidl.Handle:
		return "Handle"
	case fidl.Channel:
		return "Channel"
	case fidl.Event:
		return "Event"
	default:
		panic(fmt.Sprintf("unsupported handle subtype: %s", subtype))
	}
}

func wrapNullable(decl gidlmixer.Declaration, valueStr string) string {
	if !decl.IsNullable() {
		return valueStr
	}
	switch decl.(type) {
	case *gidlmixer.ArrayDecl, *gidlmixer.VectorDecl, *gidlmixer.StringDecl, *gidlmixer.HandleDecl:
		return fmt.Sprintf("Some(%s)", valueStr)
	case *gidlmixer.StructDecl, *gidlmixer.UnionDecl:
		return fmt.Sprintf("Some(Box::new(%s))", valueStr)
	case *gidlmixer.BoolDecl, *gidlmixer.IntegerDecl, *gidlmixer.FloatDecl, *gidlmixer.TableDecl:
		panic(fmt.Sprintf("decl %v should not be nullable", decl))
	}
	panic(fmt.Sprintf("unexpected decl %v", decl))
}

func onStruct(value gidlir.Record, decl *gidlmixer.StructDecl) string {
	var structFields []string
	providedKeys := make(map[string]struct{}, len(value.Fields))
	for _, field := range value.Fields {
		if field.Key.IsUnknown() {
			panic("unknown field not supported")
		}
		providedKeys[field.Key.Name] = struct{}{}
		fieldName := fidl.ToSnakeCase(field.Key.Name)
		fieldDecl, ok := decl.Field(field.Key.Name)
		if !ok {
			panic(fmt.Sprintf("field %s not found", field.Key.Name))
		}
		fieldValueStr := visit(field.Value, fieldDecl)
		structFields = append(structFields, fmt.Sprintf("%s: %s", fieldName, fieldValueStr))
	}
	for _, key := range decl.FieldNames() {
		if _, ok := providedKeys[key]; !ok {
			fieldName := fidl.ToSnakeCase(key)
			structFields = append(structFields, fmt.Sprintf("%s: None", fieldName))
		}
	}
	valueStr := fmt.Sprintf("%s { %s }", declName(decl), strings.Join(structFields, ", "))
	return wrapNullable(decl, valueStr)
}

func onTable(value gidlir.Record, decl *gidlmixer.TableDecl) string {
	var tableFields []string
	var unknownTuples []string
	for _, field := range value.Fields {
		if field.Key.IsUnknown() {
			unknownTuples = append(unknownTuples, fmt.Sprintf("(%d, %s)",
				field.Key.UnknownOrdinal,
				buildUnknownData(field.Value.(gidlir.UnknownData), decl.IsResourceType())))
			continue
		}
		fieldName := fidl.ToSnakeCase(field.Key.Name)
		fieldDecl, ok := decl.Field(field.Key.Name)
		if !ok {
			panic(fmt.Sprintf("field %s not found", field.Key.Name))
		}
		fieldValueStr := visit(field.Value, fieldDecl)
		tableFields = append(tableFields, fmt.Sprintf("%s: Some(%s)", fieldName, fieldValueStr))
	}
	if len(unknownTuples) > 0 {
		// When https://github.com/rust-lang/rust/issues/25725 is fixed,
		// using into_iter() on the vec! can be changed to use [T;N]::into_iter instead.
		tableFields = append(tableFields,
			fmt.Sprintf("unknown_data: Some(vec![%s].into_iter().collect())",
				strings.Join(unknownTuples, "\n")))
	} else {
		tableFields = append(tableFields, "unknown_data: None")
	}
	tableName := declName(decl)
	tableFields = append(tableFields, fmt.Sprintf("..%s::EMPTY", tableName))
	valueStr := fmt.Sprintf("%s { %s }", tableName, strings.Join(tableFields, ", "))
	return wrapNullable(decl, valueStr)
}

func onUnion(value gidlir.Record, decl *gidlmixer.UnionDecl) string {
	if len(value.Fields) != 1 {
		panic(fmt.Sprintf("union has %d fields, expected 1", len(value.Fields)))
	}
	field := value.Fields[0]
	var valueStr string
	if field.Key.IsUnknown() {
		unknownData := field.Value.(gidlir.UnknownData)
		valueStr = fmt.Sprintf(
			"%s::unknown(%d, %s)",
			declName(decl),
			field.Key.UnknownOrdinal,
			buildUnknownData(unknownData, decl.IsResourceType()),
		)
	} else {
		fieldName := fidl.ToUpperCamelCase(field.Key.Name)
		fieldDecl, ok := decl.Field(field.Key.Name)
		if !ok {
			panic(fmt.Sprintf("field %s not found", field.Key.Name))
		}
		fieldValueStr := visit(field.Value, fieldDecl)
		valueStr = fmt.Sprintf("%s::%s(%s)", declName(decl), fieldName, fieldValueStr)
	}
	return wrapNullable(decl, valueStr)
}

func onList(value []interface{}, decl gidlmixer.ListDeclaration) string {
	var elements []string
	elemDecl := decl.Elem()
	for _, item := range value {
		elements = append(elements, visit(item, elemDecl))
	}
	elementsStr := strings.Join(elements, ", ")
	switch decl.(type) {
	case *gidlmixer.ArrayDecl:
		return fmt.Sprintf("[%s]", elementsStr)
	case *gidlmixer.VectorDecl:
		return fmt.Sprintf("vec![%s]", elementsStr)
	}
	panic(fmt.Sprintf("unexpected decl %v", decl))
}

func buildHandleValue(handle gidlir.Handle) string {
	return fmt.Sprintf("unsafe { copy_handle(&handle_defs[%d]) }", handle)
}
