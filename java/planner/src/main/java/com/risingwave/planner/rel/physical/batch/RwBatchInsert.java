package com.risingwave.planner.rel.physical.batch;

import static com.risingwave.planner.rel.logical.RisingWaveLogicalRel.LOGICAL;

import com.google.common.collect.ImmutableList;
import com.google.protobuf.Any;
import com.risingwave.catalog.ColumnCatalog;
import com.risingwave.catalog.TableCatalog;
import com.risingwave.planner.rel.common.dist.RwDistributions;
import com.risingwave.planner.rel.logical.RwLogicalInsert;
import com.risingwave.proto.plan.InsertNode;
import com.risingwave.proto.plan.PlanNode;
import com.risingwave.rpc.Messages;
import java.util.List;
import java.util.Optional;
import org.apache.calcite.plan.RelOptCluster;
import org.apache.calcite.plan.RelOptRule;
import org.apache.calcite.plan.RelOptTable;
import org.apache.calcite.plan.RelTraitSet;
import org.apache.calcite.prepare.Prepare;
import org.apache.calcite.rel.RelNode;
import org.apache.calcite.rel.convert.ConverterRule;
import org.apache.calcite.rel.core.TableModify;
import org.checkerframework.checker.nullness.qual.Nullable;

/**
 * Physical version insert operator.
 *
 * @see RwLogicalInsert
 */
public class RwBatchInsert extends TableModify implements RisingWaveBatchPhyRel {
  protected RwBatchInsert(
      RelOptCluster cluster,
      RelTraitSet traitSet,
      RelOptTable table,
      Prepare.CatalogReader catalogReader,
      RelNode input,
      @Nullable List<String> updateColumnList) {
    super(
        cluster,
        traitSet,
        table,
        catalogReader,
        input,
        Operation.INSERT,
        updateColumnList,
        null,
        false);
    checkConvention();
  }

  @Override
  public TableModify copy(RelTraitSet traitSet, List<RelNode> inputs) {
    return new RwBatchInsert(
        getCluster(),
        traitSet,
        getTable(),
        getCatalogReader(),
        sole(inputs),
        getUpdateColumnList());
  }

  @Override
  public RelNode convertToDistributed() {
    return copy(
        getTraitSet().replace(BATCH_DISTRIBUTED).plus(RwDistributions.SINGLETON),
        ImmutableList.of(
            RelOptRule.convert(
                input,
                input.getTraitSet().replace(BATCH_DISTRIBUTED).plus(RwDistributions.SINGLETON))));
  }

  @Override
  public PlanNode serialize() {
    TableCatalog tableCatalog = getTable().unwrapOrThrow(TableCatalog.class);
    ImmutableList<ColumnCatalog.ColumnId> columnIds =
        Optional.ofNullable(getUpdateColumnList())
            .map(
                columns ->
                    columns.stream()
                        .map(tableCatalog::getColumnChecked)
                        .map(ColumnCatalog::getId)
                        .collect(ImmutableList.toImmutableList()))
            .orElseGet(ImmutableList::of);

    InsertNode.Builder insertNodeBuilder =
        InsertNode.newBuilder().setTableRefId(Messages.getTableRefId(tableCatalog.getId()));
    for (ColumnCatalog columnCatalog : tableCatalog.getAllColumnCatalogs()) {
      insertNodeBuilder.addColumnIds(columnCatalog.getId().getValue());
    }

    return PlanNode.newBuilder()
        .setNodeType(PlanNode.PlanNodeType.INSERT)
        .setBody(Any.pack(insertNodeBuilder.build()))
        .addChildren(((RisingWaveBatchPhyRel) input).serialize())
        .build();
  }

  /** Insert converter rule between logical and physical. */
  public static class BatchInsertConverterRule extends ConverterRule {
    public static final BatchInsertConverterRule INSTANCE =
        Config.INSTANCE
            .withInTrait(LOGICAL)
            .withOutTrait(BATCH_PHYSICAL)
            .withRuleFactory(BatchInsertConverterRule::new)
            .withOperandSupplier(t -> t.operand(RwLogicalInsert.class).anyInputs())
            .withDescription("Converting batch insert")
            .as(Config.class)
            .toRule(BatchInsertConverterRule.class);

    protected BatchInsertConverterRule(Config config) {
      super(config);
    }

    @Override
    public @Nullable RelNode convert(RelNode rel) {
      RwLogicalInsert logicalInsert = (RwLogicalInsert) rel;
      RelTraitSet requiredInputTraits =
          logicalInsert.getInput().getTraitSet().replace(BATCH_PHYSICAL);
      RelNode newInput = RelOptRule.convert(logicalInsert.getInput(), requiredInputTraits);
      return new RwBatchInsert(
          rel.getCluster(),
          rel.getTraitSet().plus(BATCH_PHYSICAL),
          logicalInsert.getTable(),
          logicalInsert.getCatalogReader(),
          newInput,
          logicalInsert.getUpdateColumnList());
    }
  }
}
